#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
helper="$repo_root/container/rootfs/usr/local/libexec/xenoteer/materialize-desktop-profile"
assets="$repo_root/container/rootfs/usr/share/xenoteer/profiles"
test_root=$(mktemp -d /tmp/xenoteer-desktop-profiles.XXXXXX)
trap 'rm -rf -- "$test_root"' EXIT
export CONTAINER_TEST_UID
export CONTAINER_TEST_GID
CONTAINER_TEST_UID=$(id -u)
CONTAINER_TEST_GID=$(id -g)

install -d "$test_root/usr/share/xenoteer" "$test_root/home/xenoteer/.cache/sessions" \
  "$test_root/home/xenoteer/.config/autostart" "$test_root/run/user/1000"
cp -a "$assets" "$test_root/usr/share/xenoteer/profiles"
printf 'must survive\n' >"$test_root/home/xenoteer/.cache/sessions/persistent-session"
printf 'must survive\n' >"$test_root/home/xenoteer/.config/autostart/persistent.desktop"

assert_home_untouched() {
  grep -Fxq 'must survive' "$test_root/home/xenoteer/.cache/sessions/persistent-session"
  grep -Fxq 'must survive' "$test_root/home/xenoteer/.config/autostart/persistent.desktop"
}

materialize() {
  CONTAINER_TEST_ROOT="$test_root" DESKTOP_PROFILE=$1 "$helper"
  test "$(stat -c '%a:%u:%g' "$test_root/run/user/1000/xdg")" = \
    "700:$CONTAINER_TEST_UID:$CONTAINER_TEST_GID"
  test "$(find "$test_root/run/user/1000/xdg/cache/sessions" -mindepth 1 -print -quit)" = ''
  test "$(cat "$test_root/run/user/1000/xdg/cache/xenoteer/desktop-profile")" = "$1"
  assert_home_untouched
}

materialize bare
printf 'runtime contamination\n' >"$test_root/run/user/1000/xdg/cache/sessions/stale"
printf 'runtime contamination\n' >"$test_root/run/user/1000/xdg/config/stale"
materialize bare
test ! -e "$test_root/run/user/1000/xdg/config/stale"

materialize standard
test -f "$test_root/run/user/1000/xdg/config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"

printf 'previous valid tree\n' >"$test_root/run/user/1000/xdg/previous-valid"
if CONTAINER_TEST_ROOT="$test_root" DESKTOP_PROFILE=invalid "$helper" 2>/dev/null; then
  printf 'profile materializer accepted an invalid profile\n' >&2
  exit 1
fi
grep -Fxq 'previous valid tree' "$test_root/run/user/1000/xdg/previous-valid"

ln -s /etc/passwd "$test_root/usr/share/xenoteer/profiles/bare/xdg/config/escape"
if CONTAINER_TEST_ROOT="$test_root" DESKTOP_PROFILE=bare "$helper" 2>/dev/null; then
  printf 'profile materializer accepted a symlinked asset\n' >&2
  exit 1
fi
grep -Fxq 'previous valid tree' "$test_root/run/user/1000/xdg/previous-valid"
rm "$test_root/usr/share/xenoteer/profiles/bare/xdg/config/escape"

doctor="$repo_root/container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/browser-runtime-doctor"
mock_bin="$test_root/mock-bin"
mock_shm="$test_root/mock-shm"
install -d -m 1777 "$mock_bin" "$mock_shm"
printf '1\n' >"$test_root/userns-clone"

cat >"$mock_bin/df" <<'MOCK'
#!/bin/sh
printf '1B-blocks Available\n4294967296 4294967296\n'
MOCK
chmod 0755 "$mock_bin/df"
PATH="$mock_bin:$PATH" SHM_PATH="$mock_shm" \
  BROWSER_EXPECTED_UID="$(id -u)" USERNS_CLONE_PATH="$test_root/userns-clone" \
  "$doctor" | grep -Fq '"ok":true'

if PATH="$mock_bin:$PATH" SHM_PATH="$mock_shm" \
  BROWSER_EXPECTED_UID="$(id -u)" USERNS_CLONE_PATH="$test_root/userns-clone" \
  CHROMIUM_USER_FLAGS='--no-sandbox' "$doctor" >/dev/null 2>&1; then
  printf 'browser doctor accepted a sandbox-disabling flag\n' >&2
  exit 1
fi

cat >"$mock_bin/df" <<'MOCK'
#!/bin/sh
printf '1B-blocks Available\n2147483648 2147483648\n'
MOCK
chmod 0755 "$mock_bin/df"
PATH="$mock_bin:$PATH" SHM_PATH="$mock_shm" \
  BROWSER_EXPECTED_UID="$(id -u)" USERNS_CLONE_PATH="$test_root/userns-clone" \
  "$doctor" --warn 2>"$test_root/browser-warning" | grep -Fq '"ok":false'
grep -Fq 'browser runtime warning:' "$test_root/browser-warning"

python3 - "$assets" <<'PY'
import configparser
import json
import pathlib
import sys
import xml.etree.ElementTree as ET

root = pathlib.Path(sys.argv[1])

def properties(path: pathlib.Path) -> dict[str, tuple[str | None, list[str]]]:
    document = ET.parse(path)
    result: dict[str, tuple[str | None, list[str]]] = {}

    def walk(element: ET.Element, prefix: str = "") -> None:
        for child in element.findall("property"):
            name = child.attrib["name"]
            key = f"{prefix}/{name}"
            result[key] = (
                child.attrib.get("value"),
                [value.attrib["value"] for value in child.findall("value")],
            )
            walk(child, key)

    walk(document.getroot())
    return result

common_xml = root / "common/xdg/config/xfce4/xfconf/xfce-perchannel-xml"
xfwm = properties(common_xml / "xfwm4.xml")
required_xfwm = {
    "/general/use_compositing": "false",
    "/general/workspace_count": "1",
    "/general/click_to_focus": "true",
    "/general/focus_new": "true",
    "/general/raise_on_focus": "false",
    "/general/prevent_focus_stealing": "false",
    "/general/wrap_workspaces": "false",
    "/general/wrap_windows": "false",
    "/general/cycle_workspaces": "false",
    "/general/scroll_workspaces": "false",
    "/general/tile_on_move": "false",
    "/general/theme": "Greybird",
    "/general/title_font": "DejaVu Sans Bold 10",
}
for key, expected in required_xfwm.items():
    assert xfwm[key][0] == expected, (key, xfwm.get(key))

xsettings = properties(common_xml / "xsettings.xml")
assert xsettings["/Net/ThemeName"][0] == "Greybird"
assert xsettings["/Net/IconThemeName"][0] == "Adwaita"
assert xsettings["/Gtk/FontName"][0] == "DejaVu Sans 10"
assert xsettings["/Gtk/MonospaceFontName"][0] == "DejaVu Sans Mono 10"
assert xsettings["/Xft/DPI"][0] == "96"
assert xsettings["/Xft/Antialias"][0] == "1"
assert xsettings["/Xft/Hinting"][0] == "1"

desktop = properties(common_xml / "xfce4-desktop.xml")
assert desktop["/backdrop/single-workspace-mode"][0] == "true"
assert desktop["/backdrop/single-workspace-number"][0] == "0"
assert desktop["/desktop-icons/style"][0] == "0"
assert desktop["/desktop-menu/show"][0] == "false"
assert desktop["/windowlist-menu/show"][0] == "false"
assert desktop["/backdrop/screen0/monitorscreen/workspace0/last-image"][0] == \
    "/usr/share/xenoteer/profiles/common/wallpaper.svg"

keyboard = properties(common_xml / "keyboard-layout.xml")
assert keyboard["/Default/XkbModel"][0] == "pc105"
assert keyboard["/Default/XkbLayout"][0] == "us"
assert keyboard["/Default/XkbVariant"][0] == ""
assert keyboard["/Default/XkbOptions"][0] == ""

for relative in ("gtk-3.0/settings.ini", "gtk-4.0/settings.ini"):
    parser = configparser.ConfigParser(interpolation=None)
    parser.read(root / "common/xdg/config" / relative)
    settings = parser["Settings"]
    assert settings["gtk-theme-name"] == "Greybird"
    assert settings["gtk-icon-theme-name"] == "Adwaita"
    assert settings["gtk-font-name"] == "DejaVu Sans 10"
    assert settings.getboolean("gtk-enable-animations") is False

for profile, expected in (("bare", ["xfwm4", "xfsettingsd", "xfdesktop"]),
                          ("standard", ["xfwm4", "xfsettingsd", "xfce4-panel", "xfdesktop"])):
    session_path = root / profile / "xdg/config/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml"
    session = properties(session_path)
    assert session["/general/SaveOnExit"][0] == "false"
    assert session["/general/PromptOnLogout"][0] == "false"
    assert session["/compat/LaunchGNOME"][0] == "false"
    assert session["/compat/LaunchKDE"][0] == "false"
    assert session["/startup/ssh-agent/enabled"][0] == "false"
    assert session["/startup/gpg-agent/enabled"][0] == "false"
    assert session["/sessions/Failsafe/IsFailsafe"][0] == "true"
    assert session["/sessions/Failsafe/Count"][0] == str(len(expected))
    commands = [session[f"/sessions/Failsafe/Client{i}_Command"][1] for i in range(len(expected))]
    assert [command[0] for command in commands] == expected
    assert commands[0] == ["xfwm4", "--compositor=off"]
    assert all("Thunar" not in command for command in commands)

autostart = root / "common/xdg/config/autostart"
for basename in (
    "at-spi-dbus-bus.desktop",
    "xfsettingsd.desktop",
    "xfce4-power-manager.desktop",
    "xfce4-screensaver.desktop",
    "light-locker.desktop",
    "xfce4-notifyd.desktop",
    "gnome-keyring-ssh.desktop",
    "gnome-keyring-gpg.desktop",
    "gnome-keyring-pkcs11.desktop",
    "gnome-keyring-secrets.desktop",
):
    parser = configparser.ConfigParser(interpolation=None)
    parser.read(autostart / basename)
    assert parser["Desktop Entry"].getboolean("Hidden") is True

panel = properties(
    root / "standard/xdg/config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"
)
assert panel["/panels"][1] == ["1"]
assert panel["/panels/panel-1/position"][0] == "p=6;x=960;y=14"
assert panel["/panels/panel-1/position-locked"][0] == "true"
assert panel["/panels/panel-1/autohide-behavior"][0] == "0"
assert panel["/plugins/plugin-1"][0] == "applicationsmenu"
assert panel["/plugins/plugin-2"][0] == "tasklist"
assert panel["/plugins/plugin-3"][0] == "separator"

chromium = json.loads((root / "common/xdg/data/xenoteer/browser-profiles/chromium/Default/Preferences").read_text())
assert chromium["browser"]["check_default_browser"] is False
assert chromium["profile"]["exit_type"] == "Normal"
assert chromium["profile"]["exited_cleanly"] is True
assert chromium["credentials_enable_service"] is False
assert chromium["profile"]["default_content_setting_values"]["notifications"] == 2
assert chromium["distribution"]["skip_first_run_ui"] is True
assert chromium["session"]["restore_on_startup"] == 5
local_state = json.loads((root / "common/xdg/data/xenoteer/browser-profiles/chromium/Local State").read_text())
assert local_state["metrics"]["reporting_enabled"] is False
assert local_state["background_mode"]["enabled"] is False

firefox = (root / "common/xdg/data/xenoteer/browser-profiles/firefox/user.js").read_text()
for required in (
    'user_pref("browser.sessionstore.resume_from_crash", false);',
    'user_pref("browser.shell.checkDefaultBrowser", false);',
    'user_pref("browser.startup.page", 0);',
    'user_pref("datareporting.policy.dataSubmissionEnabled", false);',
    'user_pref("toolkit.telemetry.enabled", false);',
    'user_pref("accessibility.force_disabled", -1);',
):
    assert required in firefox

for forbidden in ("--no-sandbox", "--disable-dev-shm-usage"):
    assert forbidden not in "\n".join(path.read_text(errors="ignore") for path in root.rglob("*" ) if path.is_file())
PY

printf 'desktop profile tests passed\n'
