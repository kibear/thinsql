use gpui_kit::*;

use thinsql_app::shell::Shell;
use thinsql_app::{perf, platform_x11, single_instance, theme};

fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("ThinSQL".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Default::default(),
            size: size(px(1280.), px(800.)),
        })),
        window_min_size: Some(size(px(960.), px(640.))),
        ..Default::default()
    }
}

fn main() {
    perf::mark_start();

    // 必须在读写配置之前抢锁：两个实例同时跑会互相覆盖配置文件
    let _instance = match single_instance::InstanceGuard::acquire(
        single_instance::InstanceGuard::default_path(),
    ) {
        Ok(guard) => guard,
        Err(err) => {
            // 第二个实例只输出错误信息：语言按配置（或系统 locale）解析，
            // 这里的配置读取是只读的，不影响主实例对配置文件的写入
            let language = thinsql_core::config::AppConfig::load().settings.language;
            thinsql_app::i18n::init(language.resolve());
            eprintln!("{err}");
            return;
        }
    };

    // 日志级别来自设置（对话框中改动在下次启动时生效）；环境变量优先，便于临时调试
    let config = thinsql_core::config::AppConfig::load();
    // 界面语言：按配置解析一次（跟随系统则探测 locale），重启后生效
    thinsql_app::i18n::init(config.settings.language.resolve());
    let level = config.settings.log_level.as_str();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("thinsql_app={level},thinsql_core={level}").into()),
        )
        .init();

    // 图标是 SVG 资源，必须注册 AssetSource，否则 IconName 全部画不出来
    // （弹窗右上角的关闭按钮就是因此“看不见”）。
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            theme::init(cx);

            gpui_kit::open_window(window_options(), cx, |window, cx| {
                #[cfg(target_os = "linux")]
                platform_x11::install_window_properties();

                Shell::build(window, cx)
            })
            .expect("failed to open window");
        });
}
