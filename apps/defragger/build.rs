use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    let builder = CxxQtBuilder::new_qml_module(
        QmlModule::new("net.gootz.defragger")
            .qml_file("qml/Main.qml")
            .qml_file("qml/DriveMap.qml"),
    )
    .qt_module("Network")
    .file("src/controller.rs")
    .cpp_file("src/application_icon.cpp")
    .qrc("resources.qrc");

    // GCC 16 diagnoses a harmless incomplete-type SFINAE probe in Qt 6.11's
    // headers. Keep the suppression local to the generated C++ compilation.
    unsafe {
        builder
            .cc_builder(|cc| {
                cc.flag_if_supported("-Wno-sfinae-incomplete");
            })
            .build();
    }
}
