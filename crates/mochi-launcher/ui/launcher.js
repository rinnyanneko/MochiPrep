// Shared script for the launcher's own pages. Each page sets up only the parts it contains.
"use strict";

function invoke(command, args) {
  return window.__TAURI__.core.invoke(command, args);
}

function showError(message) {
  const error = document.getElementById("error");
  error.textContent = String(message);
  error.hidden = false;
}

function clearError() {
  document.getElementById("error").hidden = true;
}

function setUpTerms() {
  const text = document.getElementById("terms-text");
  const boxes = [document.getElementById("understand"), document.getElementById("accept")];
  const continueButton = document.getElementById("continue");
  const quitButton = document.getElementById("quit");
  const closeButton = document.getElementById("close");
  const hint = document.getElementById("hint");

  // The launcher sets this flag when the terms are reopened from the tray after acceptance:
  // there is nothing left to agree to, only to read.
  if (window.__MOCHIPREP_TERMS_READ_ONLY__ === true) {
    document.getElementById("consent").hidden = true;
    continueButton.hidden = true;
    quitButton.hidden = true;
    hint.hidden = true;
    closeButton.hidden = false;
    closeButton.addEventListener("click", () => invoke("close_terms").catch(showError));
    return;
  }

  // Consent counts only after the whole text has been on screen, so the boxes stay disabled
  // until the reader reaches the end.
  function reachedEnd() {
    return text.scrollTop + text.clientHeight >= text.scrollHeight - 4;
  }
  function unlockIfAtEnd() {
    if (!reachedEnd()) return;
    boxes.forEach((box) => { box.disabled = false; });
    hint.hidden = true;
  }
  function updateContinue() {
    continueButton.disabled = !boxes.every((box) => box.checked);
  }

  text.addEventListener("scroll", unlockIfAtEnd);
  window.addEventListener("resize", unlockIfAtEnd);
  boxes.forEach((box) => box.addEventListener("change", updateContinue));
  unlockIfAtEnd();

  continueButton.addEventListener("click", () => {
    continueButton.disabled = true;
    invoke("accept_terms").catch((error) => {
      showError(error);
      updateContinue();
    });
  });
  quitButton.addEventListener("click", () => invoke("decline_terms").catch(showError));
}

function setUpSettings() {
  const form = document.getElementById("settings-form");
  const port = document.getElementById("port");
  const autostart = document.getElementById("autostart");
  const save = document.getElementById("save");
  const saved = document.getElementById("saved");

  invoke("get_settings")
    .then((settings) => {
      port.value = String(settings.port);
      autostart.checked = settings.autostart;
    })
    .catch(showError);

  // Any edit makes an earlier "Saved." stale.
  form.addEventListener("input", () => { saved.hidden = true; });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    clearError();
    saved.hidden = true;
    save.disabled = true;
    invoke("save_settings", { port: port.value, autostart: autostart.checked })
      .then(() => { saved.hidden = false; })
      .catch(showError)
      .finally(() => { save.disabled = false; });
  });
}

function setUpImport() {
  const importButton = document.getElementById("import");
  const skipButton = document.getElementById("skip");
  const working = document.getElementById("working");

  function setBusy(busy) {
    importButton.disabled = busy;
    skipButton.disabled = busy;
    working.hidden = !busy;
  }

  importButton.addEventListener("click", async () => {
    clearError();
    setBusy(true);
    try {
      const folder = await invoke("choose_import_folder");
      // A cancelled picker leaves the offer open.
      if (folder !== null) {
        await invoke("run_import", { folder });
      }
    } catch (error) {
      showError(error);
    } finally {
      setBusy(false);
    }
  });
  skipButton.addEventListener("click", () => invoke("skip_import").catch(showError));
}

function setUpPanel() {
  const status = document.getElementById("status");
  const armed = document.getElementById("armed");
  const button = (action) => document.querySelector(`[data-action="${action}"]`);

  function render(view) {
    status.textContent = view.status;
    armed.textContent = view.armed ?? "";
    armed.hidden = view.armed === null;
    button("open").disabled = !view.canOpenInterface;
    button("disarm").disabled = !view.canDisarmAll;
  }

  // Listening first, so a change between the two calls is not lost. Every state change emits an
  // event, and the event and the command answer travel separately, so the answer can arrive
  // after a newer event: once any event has arrived, the answer is older news and is dropped.
  let sawEvent = false;
  window.__TAURI__.event.listen("panel-state", (event) => {
    sawEvent = true;
    render(event.payload);
  })
    .then(() => invoke("panel_state"))
    .then((view) => {
      if (!sawEvent) render(view);
    })
    .catch(showError);
  document.querySelectorAll("[data-action]").forEach((element) => {
    element.addEventListener("click", () => {
      clearError();
      invoke("panel_action", { action: element.dataset.action }).catch(showError);
    });
  });
}

if (document.body.classList.contains("terms")) {
  setUpTerms();
} else if (document.body.classList.contains("settings")) {
  setUpSettings();
} else if (document.body.classList.contains("import")) {
  setUpImport();
} else if (document.body.classList.contains("panel")) {
  setUpPanel();
}
