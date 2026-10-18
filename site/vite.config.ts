import vue from "@vitejs/plugin-vue";
import { defineConfig, lazyPlugins } from "vite-plus";
import { release } from "./plugins/release.ts";

export default defineConfig({
  base: "./",
  build: {
    target: "es2023",
    assetsInlineLimit: 0,
  },
  fmt: {
    ignorePatterns: ["dist/**"],
    sortPackageJson: true,
  },
  lint: {
    ignorePatterns: ["dist/**"],
    plugins: ["typescript", "unicorn", "oxc", "vue", "import"],
    categories: {
      correctness: "error",
      suspicious: "error",
      perf: "error",
    },
    jsPlugins: [{ name: "vite-plus", specifier: "vite-plus/oxlint-plugin" }],
    rules: {
      "vite-plus/prefer-vite-plus-imports": "error",
      "typescript/no-explicit-any": "error",
      "typescript/no-non-null-assertion": "error",
      "typescript/consistent-type-imports": ["error", { disallowTypeAnnotations: false }],
      "typescript/strict-boolean-expressions": "error",
      "typescript/switch-exhaustiveness-check": "error",
      "typescript/no-unnecessary-condition": "error",
      "typescript/no-floating-promises": "error",
      "import/no-cycle": "error",
      "import/no-unassigned-import": ["error", { allow: ["**/*.css"] }],
    },
    options: { typeAware: true, typeCheck: true },
  },
  plugins: lazyPlugins(() => [vue(), release()]),
});
