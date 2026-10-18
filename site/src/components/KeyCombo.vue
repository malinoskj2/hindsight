<script setup lang="ts">
defineProps<{ keys: readonly string[] }>();
</script>

<template>
  <span class="combo">
    <template v-for="(key, index) in keys" :key="key">
      <template v-if="index > 0">+</template>
      <kbd class="key">{{ key }}</kbd>
    </template>
  </span>
</template>

<style scoped>
.combo {
  white-space: nowrap;
}

.key {
  display: inline-block;
  min-width: 1.9em;
  margin-inline: 0.1em;
  padding: 0.1em 0.45em 0.15em;
  border: 1px solid var(--ink);
  border-radius: 4px;
  box-shadow: 0 2px 0 var(--ink);
  background: var(--paper-bright);
  font: 500 0.82em/1.3 var(--mono);
  text-align: center;
  transform: translateY(-0.1em);
}
</style>
