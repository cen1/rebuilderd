#!/bin/sh
# Unset D-Bus environment variables to avoid systemd-nspawn trying to connect
unset DBUS_SESSION_BUS_ADDRESS DBUS_SYSTEM_BUS_ADDRESS

# Execute the main command
exec "$@"
