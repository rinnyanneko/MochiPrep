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

if (document.body.classList.contains("terms")) {
  setUpTerms();
}
