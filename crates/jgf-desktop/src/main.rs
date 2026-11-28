//! jg-factory-desktop: the Dark Factory window.

mod app;
mod appearance;
mod style;

fn main() -> iced::Result {
    iced::application(app::App::new, app::App::update, app::App::view)
        .title(app::App::title)
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1180.0, 780.0),
            min_size: Some(iced::Size::new(760.0, 480.0)),
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "jg-factory-desktop".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}
