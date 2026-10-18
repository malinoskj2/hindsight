<script setup lang="ts">
defineProps<{
  id: string;
  index: number;
  label: string;
  title: string;
}>();

defineSlots<{ default(): unknown }>();
</script>

<template>
  <section :id="id" class="section" :aria-labelledby="`${id}-title`">
    <p class="label">
      <span class="index">{{ String(index).padStart(2, "0") }}</span>
      {{ label }}
    </p>
    <div class="body">
      <h2 :id="`${id}-title`" class="title">{{ title }}</h2>
      <slot />
    </div>
  </section>
</template>

<style scoped>
.section {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 0.75rem;
  padding-block: 3.5rem;
  border-top: 1px solid var(--rule);
}

.label {
  margin: 0;
  color: var(--ink-soft);
  font: 500 0.75rem/1.6 var(--mono);
  letter-spacing: 0.08em;
  text-transform: uppercase;
}

.index {
  color: var(--accent);
  margin-right: 0.5em;
}

.title {
  margin: 0 0 1.75rem;
  font: 400 clamp(1.75rem, 1.3rem + 1.6vw, 2.4rem) / 1.1 var(--serif);
  letter-spacing: -0.015em;
  text-wrap: balance;
}

@media (width >= 52rem) {
  .section {
    grid-template-columns: 11rem minmax(0, 1fr);
    gap: 2rem;
  }

  .label {
    padding-top: 0.6rem;
  }
}
</style>
