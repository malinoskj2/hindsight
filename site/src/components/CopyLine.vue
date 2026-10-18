<script setup lang="ts">
import { onBeforeUnmount, ref } from "vue";

const { text } = defineProps<{ label: string; text: string }>();

type State = "idle" | "copied" | "failed";
const state = ref<State>("idle");
let reset: ReturnType<typeof setTimeout> | undefined;

const copy = async (): Promise<void> => {
  try {
    await navigator.clipboard.writeText(text);
    state.value = "copied";
  } catch {
    state.value = "failed";
  }
  clearTimeout(reset);
  reset = setTimeout(() => (state.value = "idle"), 1600);
};

const caption: Record<State, string> = { idle: "Copy", copied: "Copied", failed: "Select it" };

onBeforeUnmount(() => clearTimeout(reset));
</script>

<template>
  <figure class="line">
    <figcaption class="label">{{ label }}</figcaption>
    <div class="row">
      <code class="text">{{ text }}</code>
      <button type="button" class="copy" @click="copy">{{ caption[state] }}</button>
    </div>
  </figure>
</template>

<style scoped>
.line {
  min-width: 0;
  margin: 0;
}

.label {
  margin-bottom: 0.4rem;
  color: var(--ink-soft);
  font: 500 0.72rem/1.4 var(--mono);
  letter-spacing: 0.06em;
  text-transform: uppercase;
}

.row {
  display: flex;
  align-items: stretch;
  border: 1px solid var(--rule-strong);
  border-radius: 6px;
  background: var(--paper-bright);
}

.text {
  flex: 1;
  min-width: 0;
  padding: 0.7rem 0.9rem;
  overflow-x: auto;
  font: 400 0.8rem/1.5 var(--mono);
  white-space: nowrap;
  scrollbar-width: thin;
}

.copy {
  flex: none;
  width: 6.5rem;
  border: 0;
  border-left: 1px solid var(--rule-strong);
  border-radius: 0 5px 5px 0;
  background: transparent;
  color: var(--ink);
  font: 500 0.75rem/1 var(--mono);
  cursor: pointer;
}

.copy:hover {
  background: var(--paper-deep);
}

.copy:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: -2px;
}
</style>
