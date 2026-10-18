import { renderToString } from "vue/server-renderer";
import { createHindsightApp } from "./app.ts";

export const render = (): Promise<string> => renderToString(createHindsightApp());
