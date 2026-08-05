import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import App from "@/App";
import "@/styles.css";

const colorScheme = window.matchMedia("(prefers-color-scheme: dark)");
const syncColorScheme = (event: MediaQueryList | MediaQueryListEvent) => {
  document.documentElement.classList.toggle("dark", event.matches);
};
syncColorScheme(colorScheme);
colorScheme.addEventListener("change", syncColorScheme);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
