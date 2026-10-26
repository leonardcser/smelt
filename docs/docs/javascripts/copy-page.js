// "Copy page" button: copies the page's Markdown source to the clipboard.
// Delegated on document so it keeps working across instant navigation.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy-page]");
  if (!button) return;
  event.preventDefault();

  const idle = "Copy page as Markdown";
  let state = "copied";
  let label = "Copied";
  try {
    const response = await fetch(button.dataset.copyPage);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    await navigator.clipboard.writeText(await response.text());
  } catch {
    state = "failed";
    label = "Couldn't copy";
  }

  button.dataset.state = state;
  button.title = label;
  button.setAttribute("aria-label", label);
  clearTimeout(button.resetTimer);
  button.resetTimer = setTimeout(() => {
    delete button.dataset.state;
    button.title = idle;
    button.setAttribute("aria-label", idle);
  }, 1800);
});
