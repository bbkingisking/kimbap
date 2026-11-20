# Reinstall into Cecco's running session

Run from the repository root when asked to install/reinstall. Preserve the existing
systemd/D-Bus configuration and video support. Installation writes outside the
workspace and restart/tests need the real session bus: use sandbox escalation.

```sh
cargo xtask release --features video
```

After a successful build, install atomically and keep a rollback copy:

```sh
set -eu
kimbap_install="$HOME/.local/opt/kimbap"
kimbap_backup="$kimbap_install/backups/$(date -u +before-reinstall-%Y%m%dT%H%M%SZ)"
mkdir -p "$kimbap_backup" "$kimbap_install/share/pdf-licenses"
for binary in kimbap kimbap-jpeg-worker kimbap-pdf-worker; do
    cp -p "$kimbap_install/bin/$binary" "$kimbap_backup/$binary"
    install -m755 "target/release/$binary" "$kimbap_install/bin/.$binary.new"
    mv -f "$kimbap_install/bin/.$binary.new" "$kimbap_install/bin/$binary"
done
install -m644 data/pdf-notices.txt "$kimbap_install/share/pdf-licenses/third-party.txt"
systemctl --user restart kimbap.service
systemctl --user is-active kimbap.service
gdbus call --session --dest org.gnome.NautilusPreviewer \
    --object-path /org/gnome/NautilusPreviewer --method org.freedesktop.DBus.Peer.Ping
KIMBAP_PDF_WORKER="$kimbap_install/bin/kimbap-pdf-worker" cargo test --test pdf -- --ignored
```

`~/.local/bin/kimbap` already points to this installation. The service runs with
`--experimental-video`; no daemon-reload is needed when replacing only binaries.
The PDF worker uses Hayro and is static musl. To roll back, atomically restore
the saved binaries, then restart.
