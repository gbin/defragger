#include <QGuiApplication>
#include <QIcon>

extern "C" void set_defragger_application_icon()
{
    QGuiApplication::setWindowIcon(
        QIcon(QStringLiteral(":/icons/net.gootz.defragger.svg")));
}
