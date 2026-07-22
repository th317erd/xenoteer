const fragment = window.location.hash;
window.history.replaceState(null, "", window.location.pathname);

const status = document.getElementById("viewer-status");
const screen = document.getElementById("viewer-screen");

function fail(message) {
  status.textContent = message;
  status.dataset.state = "failed";
}

const parameters = new URLSearchParams(fragment.startsWith("#") ? fragment.slice(1) : "");
const tickets = parameters.getAll("ticket");
const pathMatch = /^\/viewer\/([0-9a-f-]{36})\/([0-9a-f-]{36})\/$/.exec(window.location.pathname);

if (tickets.length !== 1 || !/^[A-Za-z0-9_-]{32,128}$/.test(tickets[0]) || pathMatch === null) {
  fail("This viewer link is invalid or incomplete.");
} else {
  const ticketProtocol = `xenoteer.ticket.${tickets[0]}`;
  const websocketScheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  const websocketUrl = `${websocketScheme}//${window.location.host}/v1/desktops/${pathMatch[1]}/generations/${pathMatch[2]}/viewer/ws`;

  import("/viewer/vendor/core/rfb.js").then(({ default: RFB }) => {
    const rfb = new RFB(screen, websocketUrl, {
      shared: true,
      wsProtocols: ["binary", ticketProtocol],
    });
    rfb.viewOnly = true;
    rfb.scaleViewport = true;
    rfb.clipViewport = false;
    rfb.resizeSession = false;

    rfb.addEventListener("connect", () => {
      status.textContent = "Connected in view-only mode.";
      status.dataset.state = "connected";
    });
    rfb.addEventListener("disconnect", (event) => {
      fail(event.detail.clean ? "Viewer session ended." : "Viewer connection was interrupted.");
    });
    rfb.addEventListener("credentialsrequired", () => {
      rfb.disconnect();
      fail("Viewer backend authentication failed safely.");
    });
  }).catch(() => {
    fail("Viewer assets are unavailable.");
  });
}
