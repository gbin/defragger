unsafe extern "C" {
    fn configure_defragger_desktop_identity();
    fn set_defragger_application_icon();
}

pub fn configure_desktop_identity() {
    // Development builds have no installed desktop entry for the host portal
    // to resolve. Leave Qt's desktop file name unset in that case.
    unsafe { configure_defragger_desktop_identity() }
}

pub fn set_application_icon() {
    // The C++ helper constructs QIcon from the SVG embedded in resources.qrc.
    unsafe { set_defragger_application_icon() }
}
