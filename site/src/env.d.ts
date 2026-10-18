declare module "*.vue" {
  import type { DefineComponent } from "vue";
  const component: DefineComponent;
  export default component;
}

declare module "virtual:release" {
  const release: import("./release.ts").Release | null;
  export default release;
}
