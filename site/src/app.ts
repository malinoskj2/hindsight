import { createSSRApp, type App as VueApp } from "vue";
import App from "./App.vue";
import "@fontsource-variable/newsreader/opsz.css";
import "@fontsource-variable/ibm-plex-sans/wght.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "./style.css";

export const createHindsightApp = (): VueApp => createSSRApp(App);
