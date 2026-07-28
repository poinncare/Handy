import React from "react";
import ReactDOM from "react-dom/client";
import MemoryHighlight from "./MemoryHighlight";
import "./MemoryHighlight.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <MemoryHighlight />
  </React.StrictMode>,
);
