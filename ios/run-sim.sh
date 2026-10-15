#!/bin/sh
# Dev loop for the iOS Simulator:
#   sh run-sim.sh [simulator name]
# Builds, installs and launches the app with the Worker switch forced on for
# the session. The same in-process command engine runs on the simulator as on
# the device, so simulator checks exercise the device code path.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
sim=${1:-iPhone 17}
export PATH="/opt/homebrew/bin:$PATH"

xcodebuild -project "$here/CybionWorker.xcodeproj" -scheme CybionWorker \
    -configuration Debug -destination "platform=iOS Simulator,name=$sim" build >/dev/null

app=$(ls -d ~/Library/Developer/Xcode/DerivedData/CybionWorker-*/Build/Products/Debug-iphonesimulator/CybionWorker.app | head -1)
xcrun simctl boot "$sim" 2>/dev/null || true
xcrun simctl bootstatus "$sim" -b >/dev/null
xcrun simctl terminate "$sim" io.ntnl.cybion.worker 2>/dev/null || true
xcrun simctl uninstall "$sim" io.ntnl.cybion.worker 2>/dev/null || true
xcrun simctl install "$sim" "$app"

xcrun simctl launch "$sim" io.ntnl.cybion.worker -workerEnabled YES
echo "launched; watch:" 
echo "  xcrun simctl get_app_container $sim io.ntnl.cybion.worker data"
