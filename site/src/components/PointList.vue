<script setup lang="ts">
import type { Point } from "../content.ts";

const { columns = 1 } = defineProps<{
  points: readonly Point[];
  columns?: 1 | 2;
  numbered?: boolean;
}>();
</script>

<template>
  <component :is="numbered ? 'ol' : 'ul'" class="points" :class="{ split: columns === 2 }">
    <li v-for="point in points" :key="point.title" class="point">
      <strong class="point-title">{{ point.title }}</strong>
      {{ " " }}<span class="point-body">{{ point.body }}</span>
    </li>
  </component>
</template>

<style scoped>
.points {
  display: grid;
  gap: 1.25rem 2.5rem;
  margin: 0;
  padding: 0;
  list-style: none;
  counter-reset: point;
}

.point {
  position: relative;
  padding-left: 1.6rem;
  line-height: 1.6;
  counter-increment: point;
}

.point::before {
  content: "—";
  position: absolute;
  left: 0;
  color: var(--accent);
}

ol .point::before {
  content: counter(point);
  top: 0.2em;
  font: 500 0.78rem/1.6 var(--mono);
}

.point-title {
  font-weight: 600;
}

.point-body {
  color: var(--ink-soft);
}

@media (width >= 52rem) {
  .split {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}
</style>
