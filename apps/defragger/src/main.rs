mod application_icon;
mod controller;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

fn main() {
    #[cfg(feature = "development-service")]
    if let Some(socket_path) = development_helper_socket() {
        if let Err(error) = defragger_helper::run_development_helper(&socket_path) {
            eprintln!("Defragger development helper failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    let mut app = QGuiApplication::new();
    QGuiApplication::set_desktop_file_name(&QString::from("net.gootz.defragger"));
    application_icon::set_application_icon();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/net/gootz/defragger/qml/Main.qml"));
    }
    if let Some(app) = app.as_mut() {
        app.exec();
    }
}

#[cfg(feature = "development-service")]
fn development_helper_socket() -> Option<std::path::PathBuf> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next()?;
    if arguments.next()?.to_str()? != "--defragger-development-helper" {
        return None;
    }
    let socket_path = std::path::PathBuf::from(arguments.next()?);
    arguments.next().is_none().then_some(socket_path)
}
