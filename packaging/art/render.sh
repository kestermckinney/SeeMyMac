#!/usr/bin/env bash
# Regenerates the installer artwork from the SVGs in this folder (run on a Mac after editing
# them). The images are committed, so building an installer does not run this.
#
#   packaging/macos/background.png        Installer window background (620x418 pt, @2x)
#   packaging/windows/wizard-*.bmp        Inno Setup side panel, 100/150/200 % scaling
#   packaging/windows/wizard-small-*.bmp  Inno Setup header icon, 100/150/200 % scaling
#
# The colours are SeeMyMac's own: the chrome and accent from src/view.rs and the icon's navy,
# warm white and sea green from assets/icons/seemac.svg.
set -euo pipefail
ART="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${ART}/../.." && pwd)"
render() { swift "${ART}/render.swift" "$@"; }

render "${ART}/pkg-background.svg" "${ROOT}/packaging/macos/background.png" 1240 836 144
for pct in 100 150 200; do
    render "${ART}/wizard.svg" "${ROOT}/packaging/windows/wizard-${pct}.bmp" $((164 * pct / 100)) $((314 * pct / 100))
    render "${ROOT}/assets/icons/seemac.svg" "${ROOT}/packaging/windows/wizard-small-${pct}.bmp" $((55 * pct / 100)) $((55 * pct / 100))
done
echo "Rendered the installer artwork."
