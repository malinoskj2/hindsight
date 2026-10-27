<script setup lang="ts">
import release from "virtual:release";
import owl from "./assets/owl.webp";
import status from "./assets/status.png";
import wordmark from "./assets/wordmark.webp";
import CopyLine from "./components/CopyLine.vue";
import DownloadCard from "./components/DownloadCard.vue";
import KeyCombo from "./components/KeyCombo.vue";
import PageSection from "./components/PageSection.vue";
import PointList from "./components/PointList.vue";
import { codecs, features, internals, settingGroups } from "./content.ts";
import { installerName, repository } from "./release.ts";

const nav = [
  { href: "#install", label: "Install" },
  { href: "#internals", label: "How it works" },
  { href: "#settings", label: "Settings" },
  { href: repository, label: "GitHub" },
] as const;

const hotkey = ["Alt", "F7"] as const;

const settingStarts = settingGroups.map(
  (_, index) =>
    1 + settingGroups.slice(0, index).reduce((count, group) => count + group.settings.length, 0),
);

const repositorySlug = new URL(repository).pathname.slice(1);
const hashCommand = `Get-FileHash $HOME\\Downloads\\${installerName}`;
const attestCommand = `gh attestation verify ${installerName} --repo ${repositorySlug}`;
</script>

<template>
  <header class="masthead">
    <a class="brand" href="#top" aria-label="hindsight home">
      <img :src="owl" alt="" width="36" height="36" />
      <span>hindsight</span>
    </a>
    <nav aria-label="Primary">
      <a v-for="item in nav" :key="item.href" :href="item.href">{{ item.label }}</a>
    </nav>
  </header>

  <main id="top">
    <section class="hero" aria-labelledby="hero-title">
      <div class="pitch">
        <img class="wordmark" :src="wordmark" alt="hindsight" width="520" height="183" />
        <h1 id="hero-title" class="headline">Save clips of your game after they happen.</h1>
        <div class="lede">
          <p>
            hindsight quietly records your game, keeping only the last few moments. When something
            worth keeping happens, press <KeyCombo :keys="hotkey" /> and what you just played is
            saved as a video. No need to hit record first.
          </p>
          <p>
            Like NVIDIA's Instant Replay, with less overhead: captured and encoded on the GPU. No
            overlay, no launcher, no account.
          </p>
        </div>
        <DownloadCard :release="release" />
      </div>

      <figure class="shot">
        <div class="frame">
          <img
            :src="status"
            alt="hindsight's Status tab while recording cs2 at 240 fps with AV1"
            width="472"
            height="454"
          />
        </div>
        <figcaption>Fig. 1 &mdash; Recording cs2 at 240 fps, AV1, from RAM.</figcaption>
      </figure>
    </section>

    <PageSection
      id="features"
      :index="1"
      label="What you get"
      title="Records your game. Then gets out of the way."
    >
      <PointList :points="features" :columns="2" />
    </PageSection>

    <PageSection id="install" :index="2" label="Install" title="Three steps, about a minute.">
      <ol class="steps">
        <li>
          <h3>Run the installer.</h3>
          <p>
            It is not code-signed yet, so SmartScreen may stop it the first time. Choose
            <em>More info</em>, then <em>Run anyway</em>. Prefer no installer? Unzip the portable
            build anywhere.
          </p>
        </li>
        <li>
          <h3>Launch hindsight from the Start menu.</h3>
          <p>It sits in the tray, waits for a full-screen game, and records it.</p>
        </li>
        <li>
          <h3>Press <KeyCombo :keys="hotkey" /> to save a clip.</h3>
          <p>
            It saves what you just played. Change the replay length, codec, frame rate and hotkeys
            in the Settings tab.
          </p>
        </li>
      </ol>
    </PageSection>

    <PageSection
      id="internals"
      :index="3"
      label="How it stays light"
      title="Your game keeps the CPU. The capture never leaves the GPU."
    >
      <PointList :points="internals" :columns="2" />
      <p class="aside">
        Run as administrator for the highest GPU priority. To check for yourself, compare CPU use
        and frame times in PresentMon or CapFrameX with hindsight, with Instant Replay and with
        neither.
      </p>
    </PageSection>

    <PageSection
      id="settings"
      :index="4"
      label="Settings"
      title="Each setting, in the order the app shows it."
    >
      <div class="settings">
        <section
          v-for="(group, groupIndex) in settingGroups"
          :key="group.name"
          class="setting-group"
          :aria-labelledby="`settings-${group.name.toLowerCase().replaceAll(' ', '-')}`"
        >
          <h3 :id="`settings-${group.name.toLowerCase().replaceAll(' ', '-')}`" class="group-name">
            {{ group.name }}
          </h3>
          <ol class="setting-list" :start="settingStarts[groupIndex]">
            <li v-for="setting in group.settings" :key="setting.id" class="setting">
              <p class="fields">
                <span v-for="field in setting.fields" :key="field" class="field">{{ field }}</span>
              </p>
              <p class="setting-text">
                <strong>{{ setting.title }}</strong>
                {{ " " }}<span>{{ setting.body }}</span>
              </p>
              <div v-if="setting.id === 'codec'" class="table-wrap">
                <table class="codecs">
                  <caption>
                    Picture per bit
                  </caption>
                  <thead>
                    <tr>
                      <th scope="col">Codec</th>
                      <th scope="col">Bitrate for the same picture</th>
                      <th scope="col">Cards</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr v-for="codec in codecs" :key="codec.name">
                      <th scope="row">{{ codec.name }}</th>
                      <td>{{ codec.bitrate }}</td>
                      <td>{{ codec.cards }}</td>
                    </tr>
                  </tbody>
                </table>
              </div>
            </li>
          </ol>
        </section>
      </div>
      <p class="aside">
        The full guide, with every backend and buffer option:
        <a :href="`${repository}/blob/main/docs/performance.md`">Getting the best performance</a>.
      </p>
    </PageSection>

    <PageSection
      id="verify"
      :index="5"
      label="Verify"
      title="Check that your copy is the real one."
    >
      <div class="prose">
        <p>
          hindsight's code is public, so anyone can read what it does. The installer isn't built on
          someone's own PC: GitHub's servers build it from that public code, and GitHub keeps a
          signed record of which code each file came from.
        </p>
        <p>
          That lets you confirm two things: nobody swapped or tampered with your download on its way
          to you, and it was made from the same code everyone can see, with nothing slipped in.
        </p>
      </div>

      <div class="checks">
        <div class="check">
          <h3>Quick check: compare fingerprints</h3>
          <p>
            Every file has a fingerprint, a long code that changes completely if even one byte of
            the file changes. Open PowerShell, run this, and compare what it prints with the
            fingerprint below. If they differ at all, don't run the installer.
          </p>
          <CopyLine label="Run in PowerShell" :text="hashCommand" />
          <CopyLine
            v-if="release?.installer.sha256"
            :label="`Fingerprint (SHA-256) of ${release.installer.name}`"
            :text="release.installer.sha256"
          />
        </div>

        <div class="check">
          <h3>Thorough check: ask GitHub</h3>
          <p>
            With the free <a href="https://cli.github.com/">GitHub CLI</a> installed, run this in
            the folder you downloaded to. It checks GitHub's signed record and confirms this exact
            file was built from hindsight's public code.
          </p>
          <CopyLine label="Run in a terminal" :text="attestCommand" />
        </div>
      </div>

      <p class="aside">
        This proves where the file came from, not that the code is free of bugs; that is what the
        public source is for.
        <template v-if="release">
          Fingerprints for every file: <a :href="release.checksumsUrl">SHA256SUMS.txt</a>.
        </template>
      </p>
    </PageSection>
  </main>

  <footer class="colophon">
    <p>
      hindsight is free and open source under the
      <a :href="`${repository}/blob/main/LICENSE`">BSD 2-Clause license</a>. Not affiliated with
      NVIDIA.
    </p>
    <p class="colophon-links">
      <a :href="repository">Source</a>
      <a :href="`${repository}/issues`">Report a bug</a>
      <a :href="`${repository}/releases`">All releases</a>
      <a :href="`${repository}/blob/main/CONTRIBUTING.md`">Contributing</a>
    </p>
  </footer>
</template>

<style scoped>
.masthead {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.75rem 1.5rem;
  padding-block: 1.25rem;
  border-bottom: 1px solid var(--rule);
}

.brand {
  display: inline-flex;
  align-items: center;
  gap: 0.6rem;
  color: var(--ink);
  font: 600 1.05rem/1 var(--sans);
  letter-spacing: -0.01em;
  text-decoration: none;
}

nav {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 1.4rem;
  font: 500 0.78rem/1.6 var(--mono);
}

nav a {
  color: var(--ink-soft);
  text-decoration: none;
}

nav a:hover {
  color: var(--ink);
  text-decoration: underline;
}

.hero {
  display: grid;
  gap: 3rem;
  align-items: center;
  padding-block: 3.5rem 4.5rem;
}

.pitch {
  display: grid;
  gap: 1.5rem;
  justify-items: start;
}

.wordmark {
  width: min(100%, 22rem);
  height: auto;
  margin-left: -0.4rem;
}

.headline {
  max-width: 16ch;
  margin: 0;
  font: 400 clamp(2.4rem, 1.6rem + 3.2vw, 3.9rem) / 1.02 var(--serif);
  letter-spacing: -0.025em;
  text-wrap: balance;
}

.lede {
  max-width: 34rem;
  margin: 0 0 0.75rem;
  color: var(--ink-soft);
  font-size: 1.12rem;
  line-height: 1.65;
}

.lede p {
  margin: 0 0 0.75rem;
}

.lede p:last-child {
  margin-bottom: 0;
}

.shot {
  margin: 0;
  justify-self: center;
}

.frame {
  padding: 0.6rem;
  border: 1px solid var(--rule-strong);
  border-radius: 10px;
  background: var(--paper-deep);
  box-shadow:
    0 1px 0 rgb(255 255 255 / 0.6) inset,
    0 24px 48px -28px rgb(40 30 10 / 0.45);
}

.frame img {
  display: block;
  max-width: 100%;
  height: auto;
  border: 1px solid var(--rule-strong);
  border-radius: 4px;
}

.shot figcaption {
  margin-top: 0.85rem;
  color: var(--ink-soft);
  font: 400 0.75rem/1.5 var(--mono);
  text-align: center;
}

.steps {
  display: grid;
  gap: 1.5rem;
  margin: 0;
  padding: 0;
  list-style: none;
  counter-reset: step;
}

.steps li {
  position: relative;
  padding: 1.25rem 1.4rem 1.3rem 4rem;
  border: 1px solid var(--rule);
  border-radius: 8px;
  background: rgb(255 253 248 / 0.55);
  counter-increment: step;
}

.steps li::before {
  content: counter(step);
  position: absolute;
  top: 1.15rem;
  left: 1.3rem;
  display: grid;
  place-items: center;
  width: 1.75rem;
  height: 1.75rem;
  border-radius: 50%;
  background: var(--ink);
  color: var(--paper-bright);
  font: 500 0.8rem/1 var(--mono);
}

.steps h3 {
  margin: 0 0 0.3rem;
  font: 600 1.05rem/1.5 var(--sans);
}

.steps p {
  margin: 0;
  color: var(--ink-soft);
  line-height: 1.6;
}

.aside {
  max-width: 42rem;
  margin: 2rem 0 0;
  color: var(--ink-soft);
  font-size: 0.92rem;
  line-height: 1.6;
}

.prose {
  max-width: 42rem;
  margin-bottom: 2rem;
  line-height: 1.65;
}

.prose p {
  margin: 0 0 1rem;
}

.checks {
  display: grid;
  gap: 1.5rem;
}

.check {
  display: grid;
  gap: 1rem;
  max-width: 46rem;
  padding: 1.4rem 1.5rem 1.5rem;
  border: 1px solid var(--rule);
  border-radius: 8px;
  background: rgb(255 253 248 / 0.55);
}

.check h3 {
  margin: 0;
  font: 600 1.05rem/1.4 var(--sans);
}

.check p {
  margin: 0;
  color: var(--ink-soft);
  line-height: 1.6;
}

.settings {
  display: grid;
  gap: 2.25rem;
  max-width: 46rem;
  counter-reset: setting;
}

.group-name {
  margin: 0 0 1rem;
  padding-bottom: 0.5rem;
  border-bottom: 1.5px solid var(--ink);
  color: var(--ink-soft);
  font: 500 0.72rem/1.4 var(--mono);
  letter-spacing: 0.06em;
  text-transform: uppercase;
}

.setting-list {
  display: grid;
  margin: 0;
  padding: 0;
  list-style: none;
}

.setting {
  position: relative;
  padding: 1rem 0 1.1rem 2.25rem;
  border-bottom: 1px solid var(--rule);
  counter-increment: setting;
}

.setting:first-child {
  padding-top: 0.25rem;
}

.setting::before {
  content: counter(setting, decimal-leading-zero);
  position: absolute;
  top: 1.05rem;
  left: 0;
  color: var(--accent);
  font: 500 0.78rem/1.6 var(--mono);
}

.setting:first-child::before {
  top: 0.3rem;
}

.fields {
  display: flex;
  flex-wrap: wrap;
  gap: 0.4rem;
  margin: 0 0 0.55rem;
}

.field {
  padding: 0.1rem 0.45rem;
  border: 1px solid var(--rule-strong);
  border-radius: 4px;
  background: var(--paper-bright);
  color: var(--ink);
  font: 500 0.74rem/1.5 var(--mono);
}

.setting-text {
  margin: 0;
  line-height: 1.6;
}

.setting-text strong {
  font-weight: 600;
}

.setting-text span {
  color: var(--ink-soft);
}

.table-wrap {
  margin-top: 1rem;
  overflow-x: auto;
}

.codecs {
  width: 100%;
  min-width: 18rem;
  border-collapse: collapse;
  font-size: 0.88rem;
}

.codecs caption {
  margin-bottom: 0.75rem;
  color: var(--ink-soft);
  font: 500 0.72rem/1.4 var(--mono);
  letter-spacing: 0.06em;
  text-align: left;
  text-transform: uppercase;
}

.codecs th,
.codecs td {
  padding: 0.5rem 1rem 0.5rem 0;
  border-bottom: 1px solid var(--rule);
  text-align: left;
  vertical-align: top;
}

.codecs thead th {
  border-bottom: 1.5px solid var(--ink);
  color: var(--ink-soft);
  font: 500 0.75rem/1.4 var(--mono);
}

.codecs tbody tr:last-child > * {
  border-bottom: 0;
}

.codecs tbody th {
  font: 500 0.88rem/1.5 var(--mono);
}

.colophon {
  display: grid;
  gap: 0.75rem;
  padding-block: 2.5rem 3.5rem;
  border-top: 1.5px solid var(--ink);
  color: var(--ink-soft);
  font: 400 0.8rem/1.6 var(--mono);
}

.colophon p {
  margin: 0;
}

.colophon-links {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 1.4rem;
}

@media (width >= 60rem) {
  .hero {
    grid-template-columns: minmax(0, 1.15fr) minmax(0, 1fr);
    gap: 4rem;
    padding-block: 5rem 6rem;
  }

  .shot {
    justify-self: end;
  }
}
</style>
