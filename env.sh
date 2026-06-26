# Source this before building/running the GTK app on this machine.
# The GTK4/libadwaita -dev files live in a user-local prefix (no sudo needed);
# the matching runtime .so libraries are already installed system-wide in /usr.
P="$HOME/gtk-dev"
export PKG_CONFIG_PATH="$P/usr/lib/x86_64-linux-gnu/pkgconfig:$P/usr/share/pkgconfig"
export LIBRARY_PATH="$P/usr/lib/x86_64-linux-gnu:$LIBRARY_PATH"
export LD_LIBRARY_PATH="$P/usr/lib/x86_64-linux-gnu:$LD_LIBRARY_PATH"
. "$HOME/.cargo/env"
