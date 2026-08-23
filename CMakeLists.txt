cmake_minimum_required(VERSION 3.24)
project(defragger VERSION 0.1.0 LANGUAGES NONE)

find_program(CARGO_EXECUTABLE cargo REQUIRED)

add_custom_target(defragger ALL
    COMMAND "${CARGO_EXECUTABLE}" build --release --package defragger
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
    COMMENT "Building the Rust/CXX-Qt application"
    VERBATIM
)

install(PROGRAMS "${CMAKE_CURRENT_SOURCE_DIR}/target/release/defragger"
    DESTINATION bin)
install(FILES packaging/io.github.defragger.desktop
    DESTINATION share/applications)
install(FILES packaging/io.github.defragger.metainfo.xml
    DESTINATION share/metainfo)
