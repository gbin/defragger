cmake_minimum_required(VERSION 3.24)
project(defragger VERSION 0.1.0 LANGUAGES NONE)

# GNUInstallDirs cannot infer a multiarch libdir in a language-less project.
# Keep the conventional default while allowing packagers to override it.
set(CMAKE_INSTALL_LIBDIR "lib" CACHE PATH "Object code libraries")
include(GNUInstallDirs)

find_program(CARGO_EXECUTABLE cargo REQUIRED)

add_custom_target(defragger ALL
    COMMAND "${CARGO_EXECUTABLE}" build --release --package defragger --no-default-features --features system-helper
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
    COMMENT "Building the Rust/CXX-Qt application"
    VERBATIM
)

add_custom_target(defragger-helper ALL
    COMMAND "${CARGO_EXECUTABLE}" build --release --package defragger-helper
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
    COMMENT "Building the privileged Rust analysis helper"
    VERBATIM
)

configure_file(
    packaging/io.github.defragger.Helper.service.in
    "${CMAKE_CURRENT_BINARY_DIR}/io.github.defragger.Helper.service"
    @ONLY
)
configure_file(
    packaging/defragger-helper.service.in
    "${CMAKE_CURRENT_BINARY_DIR}/defragger-helper.service"
    @ONLY
)

install(PROGRAMS "${CMAKE_CURRENT_SOURCE_DIR}/target/release/defragger"
    DESTINATION "${CMAKE_INSTALL_BINDIR}")
install(PROGRAMS "${CMAKE_CURRENT_SOURCE_DIR}/target/release/defragger-helper"
    DESTINATION "${CMAKE_INSTALL_LIBEXECDIR}")
install(FILES packaging/io.github.defragger.desktop
    DESTINATION "${CMAKE_INSTALL_DATADIR}/applications")
install(FILES packaging/io.github.defragger.metainfo.xml
    DESTINATION "${CMAKE_INSTALL_DATADIR}/metainfo")
install(FILES packaging/io.github.defragger.policy
    DESTINATION "${CMAKE_INSTALL_DATADIR}/polkit-1/actions")
install(FILES packaging/io.github.defragger.Helper.conf
    DESTINATION "${CMAKE_INSTALL_DATADIR}/dbus-1/system.d")
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/io.github.defragger.Helper.service"
    DESTINATION "${CMAKE_INSTALL_DATADIR}/dbus-1/system-services")
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/defragger-helper.service"
    DESTINATION "${CMAKE_INSTALL_LIBDIR}/systemd/system")
