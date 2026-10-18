<script setup lang="ts">
import { computed } from "vue";
import { requirements } from "../content.ts";
import { formatBytes, formatDate } from "../format.ts";
import {
  installerName,
  latestDownload,
  portableName,
  repository,
  type Release,
} from "../release.ts";

const { release } = defineProps<{ release: Release | null }>();

const installerUrl = computed(() => release?.installer.url ?? latestDownload(installerName));
const portableUrl = computed(() => release?.portable.url ?? latestDownload(portableName));
const notesUrl = computed(() => release?.notesUrl ?? `${repository}/releases/latest`);
</script>

<template>
  <div class="card">
    <a class="button" :href="installerUrl" download>
      <svg class="arrow" viewBox="0 0 16 16" aria-hidden="true">
        <path d="M8 1.5v9m0 0L4 6.5m4 4 4-4M2 14.5h12" />
      </svg>
      Download for Windows
    </a>

    <p class="meta">
      <template v-if="release">
        <span>v{{ release.version }}</span>
        <span>installer</span>
        <span>{{ formatBytes(release.installer.bytes) }}</span>
        <span>
          <time :datetime="release.publishedAt">{{ formatDate(release.publishedAt) }}</time>
        </span>
      </template>
      <template v-else>
        <span>Latest release</span>
        <span>installer</span>
      </template>
    </p>

    <ul class="links" aria-label="Other downloads">
      <li>
        <a :href="portableUrl" download>Portable .zip</a>
        <template v-if="release"> ({{ formatBytes(release.portable.bytes) }})</template>
      </li>
      <li><a :href="notesUrl">Release notes</a></li>
      <li><a href="#verify">Verify</a></li>
    </ul>

    <ul class="requirements" aria-label="Requirements">
      <li v-for="requirement in requirements" :key="requirement">{{ requirement }}</li>
    </ul>
  </div>
</template>

<style scoped>
.card {
  display: grid;
  gap: 0.9rem;
  justify-items: start;
}

.button {
  display: inline-flex;
  align-items: center;
  gap: 0.7rem;
  padding: 0.95rem 1.5rem 1rem 1.3rem;
  border: 1.5px solid var(--ink);
  border-radius: 6px;
  background: var(--ink);
  box-shadow: 4px 4px 0 var(--accent);
  color: var(--paper-bright);
  font: 600 1.08rem/1 var(--sans);
  letter-spacing: 0.005em;
  text-decoration: none;
  transition:
    transform 120ms ease,
    box-shadow 120ms ease;
}

.button:hover {
  transform: translate(-1px, -1px);
  box-shadow: 5px 5px 0 var(--accent);
}

.button:active {
  transform: translate(3px, 3px);
  box-shadow: 1px 1px 0 var(--accent);
}

.button:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 4px;
}

.arrow {
  width: 1.05em;
  height: 1.05em;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.8;
  stroke-linecap: round;
  stroke-linejoin: round;
}

.meta,
.links,
.requirements {
  display: flex;
  flex-wrap: wrap;
  margin: 0;
  padding: 0;
  font: 400 0.8rem/1.6 var(--mono);
}

.meta {
  color: var(--ink-soft);
}

.meta > span + span::before,
.links > li + li::before {
  content: "·";
  margin-inline: 0.55em;
  color: var(--ink-faint);
}

.links {
  list-style: none;
}

.requirements {
  flex-direction: column;
  margin-top: 0.4rem;
  padding-top: 0.9rem;
  border-top: 1px dashed var(--rule);
  color: var(--ink-soft);
  list-style: none;
}

.requirements li::before {
  content: "✓ ";
  color: var(--accent);
}
</style>
