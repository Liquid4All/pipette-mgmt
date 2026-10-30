import { mount } from "svelte";
import App from "./App.svelte";
import "./app.css";

// A busy button is `aria-disabled` so it keeps the focus `disabled` would drop; its click,
// and so a form's submit and Enter in a field, is refused here.
addEventListener("click", (event) => {
  if (event.target instanceof Element && event.target.closest('button[aria-disabled="true"]')) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }
}, { capture: true });

mount(App, { target: document.getElementById("app")! });
