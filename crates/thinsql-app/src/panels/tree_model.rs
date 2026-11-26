//! 左侧结构树的纯数据模型：节点 id 编解码、对象分组与搜索过滤。

use std::collections::HashSet;

use gpui_kit::component::tree::TreeItem;
use thinsql_core::model::ObjectKind;

use crate::i18n::t;
use crate::state::{ConnEntry, ConnStatus};

#[derive(Debug, Clone, PartialEq)]
pub enum NodeRef {
    Conn(String),
    Schema { conn: String, schema: String },
    Kind {
        conn: String,
        schema: String,
        kind: ObjectKind,
    },
    Object {
        conn: String,
        schema: String,
        kind: ObjectKind,
        name: String,
    },
}

pub fn conn_node_id(conn: &str) -> String {
    format!("conn:{conn}")
}

pub fn schema_node_id(conn: &str, schema: &str) -> String {
    format!("schema:{conn}:{schema}")
}

pub fn kind_node_id(conn: &str, schema: &str, kind: ObjectKind) -> String {
    format!("kind:{conn}:{schema}:{}", kind.as_str())
}

pub fn object_node_id(conn: &str, schema: &str, kind: ObjectKind, name: &str) -> String {
    format!("obj:{conn}:{schema}:{}:{name}", kind.as_str())
}

pub fn kind_label(kind: ObjectKind) -> String {
    match kind {
        ObjectKind::Table => t("表").into(),
        ObjectKind::View => t("视图").into(),
        ObjectKind::MaterializedView => t("物化视图").into(),
        ObjectKind::Procedure => t("存储过程").into(),
        ObjectKind::Function => t("函数").into(),
        ObjectKind::Sequence => t("序列").into(),
        ObjectKind::Trigger => t("触发器").into(),
        ObjectKind::Event => t("事件").into(),
    }
}

pub fn parse_node(id: &str) -> Option<NodeRef> {
    let mut parts = id.splitn(2, ':');
    match parts.next()? {
        "conn" => Some(NodeRef::Conn(parts.next()?.to_string())),
        "schema" => {
            let rest = parts.next()?;
            let mut bits = rest.splitn(2, ':');
            Some(NodeRef::Schema {
                conn: bits.next()?.to_string(),
                schema: bits.next()?.to_string(),
            })
        }
        "kind" => {
            let rest = parts.next()?;
            let mut bits = rest.splitn(3, ':');
            let conn = bits.next()?.to_string();
            let schema = bits.next()?.to_string();
            let kind = ObjectKind::parse(bits.next()?)?;
            Some(NodeRef::Kind { conn, schema, kind })
        }
        "obj" => {
            let rest = parts.next()?;
            let mut bits = rest.splitn(4, ':');
            let conn = bits.next()?.to_string();
            let schema = bits.next()?.to_string();
            let kind = ObjectKind::parse(bits.next()?)?;
            let name = bits.next()?.to_string();
            Some(NodeRef::Object {
                conn,
                schema,
                kind,
                name,
            })
        }
        _ => None,
    }
}

fn placeholder(id: String, label: &str) -> TreeItem {
    TreeItem::new(id, label.to_string()).disabled(true)
}

fn hit(query: &str, text: &str) -> bool {
    text.to_lowercase().contains(query)
}

/// 用当前状态重建树。展开状态保存在 AppState 里，重建后不会丢失。
/// 搜索只影响本次渲染：命中节点的祖先临时展开，不改持久化的展开集合。
pub fn build_items(conns: &[ConnEntry], expanded: &HashSet<String>, query: &str) -> Vec<TreeItem> {
    let query = query.trim().to_lowercase();
    let filtering = !query.is_empty();
    let mut roots = Vec::new();

    for conn in conns {
        let conn_kinds = navigator_kinds_for_conn(conn);
        let conn_id = conn_node_id(&conn.cfg.id);
        let conn_hit = filtering && hit(&query, &conn.cfg.name);
        let mut root =
            TreeItem::new(conn_id.clone(), conn.cfg.name.clone()).expanded(if filtering {
                true
            } else {
                expanded.contains(&conn_id)
            });

        if matches!(conn.status, ConnStatus::Connected) {
            for schema in &conn.schemas {
                let schema_id = schema_node_id(&conn.cfg.id, schema);
                let schema_hit = filtering && hit(&query, schema);
                let include_all = !filtering || conn_hit || schema_hit;

                let mut kind_nodes = Vec::new();
                for &kind in conn_kinds {
                    let kind_id = kind_node_id(&conn.cfg.id, schema, kind);
                    let objects = conn
                        .objects
                        .get(schema)
                        .and_then(|m| m.get(&kind));
                    let visible: Vec<String> = match objects {
                        Some(names) if include_all => names.clone(),
                        Some(names) => names
                            .iter()
                            .filter(|name| hit(&query, name))
                            .cloned()
                            .collect(),
                        None if filtering && !include_all => Vec::new(),
                        None => Vec::new(),
                    };
                    if filtering && !include_all && visible.is_empty() && objects.is_some() {
                        continue;
                    }
                    if filtering && !include_all && objects.is_none() {
                        continue;
                    }

                    let mut kind_node =
                        TreeItem::new(kind_id.clone(), kind_label(kind)).expanded(if filtering {
                            true
                        } else {
                            expanded.contains(&kind_id)
                        });
                    match objects {
                        None => {
                            kind_node = kind_node.child(placeholder(
                                format!("placeholder:{kind_id}"),
                                t("展开以加载"),
                            ));
                        }
                        Some(_) if visible.is_empty() => {
                            kind_node = kind_node.child(placeholder(
                                format!("empty:{kind_id}"),
                                t("（空）"),
                            ));
                        }
                        Some(_) => {
                            for name in visible {
                                let id =
                                    object_node_id(&conn.cfg.id, schema, kind, &name);
                                kind_node = kind_node.child(TreeItem::new(id, name));
                            }
                        }
                    }
                    kind_nodes.push(kind_node);
                }

                if filtering && !include_all && kind_nodes.is_empty() {
                    continue;
                }

                let mut node =
                    TreeItem::new(schema_id.clone(), schema.clone()).expanded(if filtering {
                        true
                    } else {
                        expanded.contains(&schema_id)
                    });
                for kind_node in kind_nodes {
                    node = node.child(kind_node);
                }
                // 库节点始终是 folder：至少挂上分组
                if node.children.is_empty() {
                    for &kind in conn_kinds {
                        let kind_id = kind_node_id(&conn.cfg.id, schema, kind);
                        node = node.child(
                            TreeItem::new(kind_id.clone(), kind_label(kind)).child(placeholder(
                                format!("placeholder:{kind_id}"),
                                t("展开以加载"),
                            )),
                        );
                    }
                }
                root = root.child(node);
            }

            if conn.schemas.is_empty() && (!filtering || conn_hit) {
                let (id, label) = if conn.schema_loaded {
                    (format!("empty:conn:{}", conn.cfg.id), t("（无库）"))
                } else {
                    (format!("placeholder:conn:{}", conn.cfg.id), t("加载中…"))
                };
                root = root.child(placeholder(id, label));
            }
        } else if !filtering || conn_hit {
            let label = if matches!(conn.status, ConnStatus::Connecting) {
                t("连接中…")
            } else {
                t("展开以连接并加载库")
            };
            root = root.child(placeholder(
                format!("placeholder:conn:{}", conn.cfg.id),
                label,
            ));
        }

        if filtering && !conn_hit && root.children.is_empty() {
            continue;
        }
        roots.push(root);
    }

    roots
}

fn navigator_kinds_for_conn(conn: &ConnEntry) -> &'static [ObjectKind] {
    thinsql_core::drivers::navigator_object_kinds(conn.cfg.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinsql_core::model::{ConnectionConfig, DriverKind, ObjectKind};
    use crate::state::ConnEntry;

    fn connected_conn() -> ConnEntry {
        let mut conn = ConnEntry::new(ConnectionConfig::new("c1", "local", DriverKind::MySql));
        conn.status = ConnStatus::Connected;
        conn.schema_loaded = true;
        conn.schemas = vec!["public".into()];
        conn
    }

    fn schema_node(items: &[TreeItem]) -> &TreeItem {
        &items[0].children[0]
    }

    #[test]
    fn schema_expands_to_kind_groups_not_tables() {
        let items = build_items(&[connected_conn()], &HashSet::new(), "");
        let schema = schema_node(&items);
        assert!(schema.is_folder());
        assert!(
            schema.children.iter().any(|c| c.id.as_ref().starts_with("kind:")),
            "库下应是对象分组"
        );
        assert!(
            schema
                .children
                .iter()
                .all(|c| !c.id.as_ref().starts_with("obj:")),
            "未展开分组前不应直接挂对象"
        );
    }

    #[test]
    fn loaded_tables_appear_under_table_kind() {
        let mut conn = connected_conn();
        conn.objects
            .entry("public".into())
            .or_default()
            .insert(ObjectKind::Table, vec!["orders".into()]);
        let items = build_items(&[conn], &HashSet::new(), "");
        let schema = schema_node(&items);
        let tables = schema
            .children
            .iter()
            .find(|c| c.id.as_ref() == "kind:c1:public:table")
            .expect("应有 Tables 分组");
        assert_eq!(tables.children.len(), 1);
        assert_eq!(
            tables.children[0].id.as_ref(),
            "obj:c1:public:table:orders"
        );
    }

    #[test]
    fn empty_kind_keeps_placeholder() {
        let mut conn = connected_conn();
        conn.objects
            .entry("public".into())
            .or_default()
            .insert(ObjectKind::View, Vec::new());
        let items = build_items(&[conn], &HashSet::new(), "");
        let schema = schema_node(&items);
        let views = schema
            .children
            .iter()
            .find(|c| c.id.as_ref() == "kind:c1:public:view")
            .expect("应有 Views 分组");
        assert!(
            views
                .children
                .iter()
                .any(|c| c.id.as_ref().starts_with("empty:")),
            "空分组应有占位"
        );
    }

    #[test]
    fn search_keeps_matching_objects_and_expands_ancestors() {
        let mut conn = connected_conn();
        conn.objects.entry("public".into()).or_default().insert(
            ObjectKind::Table,
            vec!["orders".into(), "users".into()],
        );
        let items = build_items(&[conn], &HashSet::new(), "ORD");
        assert_eq!(items.len(), 1);
        assert!(items[0].is_expanded());
        let schema = schema_node(&items);
        assert!(schema.is_expanded());
        let tables = schema
            .children
            .iter()
            .find(|c| c.id.as_ref().contains(":table"))
            .expect("命中表所属分组");
        assert_eq!(tables.children.len(), 1);
        assert!(tables.children[0].id.as_ref().ends_with(":orders"));
    }

    #[test]
    fn parses_kind_and_object_nodes() {
        let kind = kind_node_id("c1", "db", ObjectKind::Function);
        match parse_node(&kind) {
            Some(NodeRef::Kind { kind, .. }) => assert_eq!(kind, ObjectKind::Function),
            other => panic!("{other:?}"),
        }
        let obj = object_node_id("c1", "db", ObjectKind::Table, "we:ird");
        match parse_node(&obj) {
            Some(NodeRef::Object { name, kind, .. }) => {
                assert_eq!(name, "we:ird");
                assert_eq!(kind, ObjectKind::Table);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn table_ref_from_relation_object() {
        let table = TableRef::new(Some("public".into()), "orders");
        assert_eq!(table.name, "orders");
    }
}
