//! SQL 语句切分。
//!
//! 按分号切语句看起来是个 `split(';')` 的活，实际上分号会出现在：
//! 字符串字面量、带引号的标识符、行注释、块注释，以及 PostgreSQL 的美元引号里。
//! 切错的后果是把一条合法语句劈成两半，然后在服务端报一个看起来莫名其妙的语法错误。

/// 把脚本切成独立语句，去掉空语句与纯注释片段。
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        let c = bytes[i];

        // 行注释：-- 到行尾
        if c == '-' && i + 1 < n && bytes[i + 1] == '-' {
            while i < n && bytes[i] != '\n' {
                current.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        // 块注释：/* ... */，PostgreSQL 支持嵌套
        if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            let mut depth = 0usize;
            while i < n {
                if bytes[i] == '/' && i + 1 < n && bytes[i + 1] == '*' {
                    depth += 1;
                    current.push('/');
                    current.push('*');
                    i += 2;
                    continue;
                }
                if bytes[i] == '*' && i + 1 < n && bytes[i + 1] == '/' {
                    depth -= 1;
                    current.push('*');
                    current.push('/');
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                current.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        // 单引号字符串：'' 表示一个引号
        if c == '\'' {
            current.push(c);
            i += 1;
            while i < n {
                if bytes[i] == '\'' {
                    current.push('\'');
                    i += 1;
                    if i < n && bytes[i] == '\'' {
                        current.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                current.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        // 双引号标识符 / 反引号标识符：内部重复一次表示转义
        if c == '"' || c == '`' {
            let quote = c;
            current.push(c);
            i += 1;
            while i < n {
                current.push(bytes[i]);
                if bytes[i] == quote {
                    i += 1;
                    if i < n && bytes[i] == quote {
                        current.push(bytes[i]);
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        // 美元引号：$$ ... $$ 或 $tag$ ... $tag$
        if c == '$' {
            if let Some(tag) = dollar_tag(&bytes[i..]) {
                let tag_chars: Vec<char> = tag.chars().collect();
                current.extend(tag_chars.iter());
                i += tag_chars.len();
                // 找到匹配的结束标记
                while i < n {
                    if bytes[i] == '$' {
                        let rest: String = bytes[i..].iter().take(tag_chars.len()).collect();
                        if rest == tag {
                            current.push_str(&tag);
                            i += tag_chars.len();
                            break;
                        }
                    }
                    current.push(bytes[i]);
                    i += 1;
                }
                continue;
            }
        }

        if c == ';' {
            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() && !is_only_comments(&trimmed) {
                out.push(trimmed);
            }
            current.clear();
            i += 1;
            continue;
        }

        current.push(c);
        i += 1;
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() && !is_only_comments(&trimmed) {
        out.push(trimmed);
    }
    out
}

/// `$tag$` 形式：`$` + 可选标识符 + `$`。返回完整标记（含两个 `$`）。
fn dollar_tag(rest: &[char]) -> Option<String> {
    if rest.first() != Some(&'$') {
        return None;
    }
    let mut i = 1;
    while i < rest.len() {
        let c = rest[i];
        if c == '$' {
            return Some(rest[..=i].iter().collect());
        }
        // 标签只允许字母、数字、下划线，且不能以数字开头
        let ok = c.is_ascii_alphabetic() || c == '_' || (i > 1 && c.is_ascii_digit());
        if !ok {
            return None;
        }
        i += 1;
    }
    None
}

fn is_only_comments(text: &str) -> bool {
    text.lines().all(|line| {
        let t = line.trim();
        t.is_empty() || t.starts_with("--")
    })
}


/// 把关系名位置上的保留字自动加上标识符引号。
///
/// 只处理 `FROM` / `JOIN` / `INTO` / `UPDATE` / `TABLE` 后的裸标识符（含 `schema.table`），
/// 不改写 `ORDER BY` 这类关键字用法，也不动字符串/注释/已加引号的标识符。
pub fn quote_reserved_relations(sql: &str, quote: char) -> String {
    let bytes: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0usize;
    let n = bytes.len();
    let mut expect_relation = false;

    while i < n {
        let c = bytes[i];

        if c == '-' && i + 1 < n && bytes[i + 1] == '-' {
            while i < n && bytes[i] != '\n' {
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            let mut depth = 0usize;
            while i < n {
                if bytes[i] == '/' && i + 1 < n && bytes[i + 1] == '*' {
                    depth += 1;
                    out.push('/');
                    out.push('*');
                    i += 2;
                    continue;
                }
                if bytes[i] == '*' && i + 1 < n && bytes[i + 1] == '/' {
                    depth -= 1;
                    out.push('*');
                    out.push('/');
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        if c == '\'' {
            out.push(c);
            i += 1;
            while i < n {
                out.push(bytes[i]);
                if bytes[i] == '\'' {
                    i += 1;
                    if i < n && bytes[i] == '\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if c == '"' || c == '`' {
            let q = c;
            out.push(c);
            i += 1;
            while i < n {
                out.push(bytes[i]);
                if bytes[i] == q {
                    i += 1;
                    if i < n && bytes[i] == q {
                        out.push(bytes[i]);
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            if expect_relation {
                let mut j = i;
                while j < n && bytes[j].is_whitespace() {
                    j += 1;
                }
                expect_relation = j < n && bytes[j] == '.';
            }
            continue;
        }

        if c == '$' {
            if let Some(tag) = dollar_tag(&bytes[i..]) {
                let tag_chars: Vec<char> = tag.chars().collect();
                out.extend(tag_chars.iter());
                i += tag_chars.len();
                while i < n {
                    if bytes[i] == '$' {
                        let rest: String = bytes[i..].iter().take(tag_chars.len()).collect();
                        if rest == tag {
                            out.push_str(&tag);
                            i += tag_chars.len();
                            break;
                        }
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                continue;
            }
        }

        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                i += 1;
            }
            let word: String = bytes[start..i].iter().collect();
            let lower = word.to_ascii_lowercase();
            if expect_relation {
                if is_sql_reserved(&lower) {
                    out.push(quote);
                    out.push_str(&word.replace(quote, &format!("{quote}{quote}")));
                    out.push(quote);
                } else {
                    out.push_str(&word);
                }
                // schema.table：点号后继续期望关系段
                let mut j = i;
                while j < n && bytes[j].is_whitespace() {
                    j += 1;
                }
                if j < n && bytes[j] == '.' {
                    expect_relation = true;
                } else {
                    expect_relation = false;
                }
            } else {
                out.push_str(&word);
                if is_relation_introducer(&lower) {
                    expect_relation = true;
                }
            }
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

fn is_relation_introducer(word: &str) -> bool {
    matches!(
        word,
        "from" | "join" | "into" | "update" | "table"
    )
}

fn is_sql_reserved(word: &str) -> bool {
    // 覆盖常见会撞表名的保留字；只在关系名位置才引用，避免误伤 ORDER BY。
    matches!(
        word,
        "add"
            | "all"
            | "alter"
            | "analyze"
            | "and"
            | "as"
            | "asc"
            | "between"
            | "by"
            | "case"
            | "check"
            | "column"
            | "create"
            | "cross"
            | "current_date"
            | "current_time"
            | "current_timestamp"
            | "database"
            | "default"
            | "delete"
            | "desc"
            | "describe"
            | "distinct"
            | "drop"
            | "else"
            | "exists"
            | "false"
            | "for"
            | "from"
            | "full"
            | "group"
            | "having"
            | "in"
            | "index"
            | "inner"
            | "insert"
            | "into"
            | "is"
            | "join"
            | "key"
            | "left"
            | "like"
            | "limit"
            | "lock"
            | "match"
            | "natural"
            | "not"
            | "null"
            | "on"
            | "or"
            | "order"
            | "outer"
            | "primary"
            | "procedure"
            | "range"
            | "right"
            | "select"
            | "set"
            | "show"
            | "table"
            | "then"
            | "true"
            | "union"
            | "unique"
            | "update"
            | "use"
            | "user"
            | "using"
            | "values"
            | "when"
            | "where"
            | "with"
            | "xor"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_plain_semicolons() {
        let stmts = split_statements("SELECT 1; SELECT 2;");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn keeps_semicolon_inside_string_literal() {
        let stmts = split_statements("INSERT INTO t VALUES ('a;b'); SELECT 1");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a;b'"), "实际: {:?}", stmts);
    }

    #[test]
    fn escaped_quote_inside_string() {
        let stmts = split_statements("SELECT 'it''s; fine'; SELECT 2");
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
        assert!(stmts[0].contains("it''s; fine"));
    }

    #[test]
    fn keeps_semicolon_inside_comments() {
        let stmts = split_statements("SELECT 1 -- a; b\n; SELECT 2");
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
    }

    #[test]
    fn keeps_semicolon_inside_block_comment() {
        let stmts = split_statements("SELECT 1 /* a; b */; SELECT 2");
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
    }

    #[test]
    fn nested_block_comments_are_handled() {
        let stmts = split_statements("SELECT 1 /* outer /* inner; */ still */; SELECT 2");
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
    }

    #[test]
    fn dollar_quoted_body_is_one_statement() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT 2";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
        assert!(stmts[0].contains("SELECT 1;"));
    }

    #[test]
    fn tagged_dollar_quote_is_handled() {
        let sql = "DO $body$ BEGIN PERFORM 1; END $body$; SELECT 3";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
    }

    #[test]
    fn quoted_identifier_may_contain_semicolon() {
        let stmts = split_statements("SELECT * FROM \"weird;name\"; SELECT 2");
        assert_eq!(stmts.len(), 2, "实际: {stmts:?}");
    }

    #[test]
    fn trailing_semicolon_and_blank_input_are_ignored() {
        assert_eq!(split_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert!(split_statements("   ").is_empty());
        assert!(split_statements("-- just a comment").is_empty());
    }

    #[test]
    fn empty_statements_between_semicolons_are_dropped() {
        let stmts = split_statements("SELECT 1;;; SELECT 2;");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn quotes_reserved_table_after_from() {
        assert_eq!(
            quote_reserved_relations("select * from order", '`'),
            "select * from `order`"
        );
        assert_eq!(
            quote_reserved_relations("SELECT * FROM \"order\"", '"'),
            "SELECT * FROM \"order\""
        );
    }

    #[test]
    fn quotes_reserved_in_schema_qualified_name() {
        assert_eq!(
            quote_reserved_relations("select * from hishop.order", '`'),
            "select * from hishop.`order`"
        );
    }

    #[test]
    fn leaves_order_by_untouched() {
        let sql = "select * from t order by id";
        assert_eq!(quote_reserved_relations(sql, '`'), sql);
    }

    #[test]
    fn does_not_quote_where_after_reserved_table() {
        assert_eq!(
            quote_reserved_relations("select * from order where id = 1", '`'),
            "select * from `order` where id = 1"
        );
    }

    #[test]
    fn quotes_after_join_and_update() {
        assert_eq!(
            quote_reserved_relations("select * from t join `group` g on 1=1", '`'),
            "select * from t join `group` g on 1=1"
        );
        assert_eq!(
            quote_reserved_relations("update user set name=1", '`'),
            "update `user` set name=1"
        );
        assert_eq!(
            quote_reserved_relations("insert into order values (1)", '`'),
            "insert into `order` values (1)"
        );
    }
}
