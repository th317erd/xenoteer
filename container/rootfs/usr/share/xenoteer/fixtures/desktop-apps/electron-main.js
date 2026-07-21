// SPDX-License-Identifier: BUSL-1.1
"use strict";

const path = require("path");
const {app, BrowserWindow, session} = require("electron");

app.commandLine.appendSwitch("disable-background-networking");
app.commandLine.appendSwitch("disable-breakpad");
app.commandLine.appendSwitch("disable-component-update");
app.commandLine.appendSwitch("disable-gpu");
app.commandLine.appendSwitch("force-renderer-accessibility", "complete");
app.commandLine.appendSwitch("no-first-run");
app.setPath("userData", path.join(process.env.XDG_DATA_HOME, "xenoteer/browser-profiles/electron"));
app.setPath("cache", path.join(process.env.XDG_CACHE_HOME, "xenoteer/electron"));

app.whenReady().then(async () => {
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  const window = new BrowserWindow({
    width: 800,
    height: 600,
    title: "Xenoteer Electron Browser Fixture",
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
    },
  });
  await window.loadFile(path.join(__dirname, "fixture.html"), {query: {browser: "electron"}});
});

app.on("window-all-closed", () => app.quit());
