import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ClientProvider } from "@solana/react";
import "./index.css";
import App from "./App.tsx";
import { client } from "./lib/client.ts";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ClientProvider client={client}>
      <App />
    </ClientProvider>
  </StrictMode>,
);
