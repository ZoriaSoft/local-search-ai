import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import type { View } from "./components/NavRail";
import { language, setLanguage, type Language } from "./i18n";
import "./styles.css";

/** Owns the UI language: a switch remounts the app with the new strings,
 * back on the page it was made from. */
function Root() {
  const [state, setState] = useState<{ lang: Language; view?: View }>({ lang: language });
  const onLanguage = (next: Language, view: View) => {
    setLanguage(next);
    setState({ lang: next, view });
  };
  return <App key={state.lang} initialView={state.view} onLanguage={onLanguage} />;
}

const root = document.getElementById("root");
if (!root) throw new Error("#root missing from index.html");
createRoot(root).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
