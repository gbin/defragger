unsafe extern "C" {
    fn set_defragger_application_icon();
}

pub fn set_application_icon() {
    // The C++ helper constructs QIcon from the SVG embedded in resources.qrc.
    unsafe { set_defragger_application_icon() }
}
