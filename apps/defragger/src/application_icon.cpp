#include <QGuiApplication>
#include <QIcon>
#include <QStandardPaths>

extern "C" void configure_defragger_desktop_identity()
{
    const auto desktopEntry = QStandardPaths::locate(
        QStandardPaths::GenericDataLocation,
        QStringLiteral("applications/net.gootz.defragger.desktop"));
    if (!desktopEntry.isEmpty()) {
        QGuiApplication::setDesktopFileName(
            QStringLiteral("net.gootz.defragger"));
    }
}

extern "C" void set_defragger_application_icon()
{
    QGuiApplication::setWindowIcon(
        QIcon(QStringLiteral(":/icons/net.gootz.defragger.svg")));
}
