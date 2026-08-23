use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("io.github.defragger")
            .qml_file("qml/Main.qml")
            .qml_file("qml/DriveMap.qml"),
    )
    .qt_module("Network")
    .file("src/controller.rs")
    .build();
}
