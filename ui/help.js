/* llmproxy admin — user guide behaviour.
   Two small things, both of which a long reference page needs and neither of
   which is worth a dependency: a contents rail that tracks the reading
   position, and a filter that hides features that do not match a search.
   Everything works without this file; it only sharpens navigation. */

const links = Array.from(document.querySelectorAll(".toc-link"));
const targets = links
  .map((link) => document.getElementById(link.getAttribute("href").slice(1)))
  .filter(Boolean);

/* Contents highlighting.
   The heading nearest the top of the viewport wins rather than whichever
   section happens to intersect: with short cards several are on screen at
   once, and "most recently passed" is what a reader means by "where I am". */
function syncActiveLink() {
  const line = 120;
  let active = targets[0];
  for (const target of targets) {
    if (target.getBoundingClientRect().top <= line) active = target;
  }
  for (const link of links) {
    const matches = active && link.getAttribute("href") === `#${active.id}`;
    if (matches) link.setAttribute("aria-current", "true");
    else link.removeAttribute("aria-current");
  }
}

let scrollQueued = false;
window.addEventListener(
  "scroll",
  () => {
    if (scrollQueued) return;
    scrollQueued = true;
    requestAnimationFrame(() => {
      scrollQueued = false;
      syncActiveLink();
    });
  },
  { passive: true }
);

/* Filtering.
   Each feature card carries a data-terms list of the config keys, verbs, and
   synonyms a reader is likely to arrive with — "429", "rate limit", "secret" —
   because the words in a heading are rarely the words someone searches for. */
const features = Array.from(document.querySelectorAll(".feature"));
const sections = Array.from(document.querySelectorAll(".help-section"));
const filter = document.getElementById("help-filter");
const empty = document.getElementById("filter-empty");

function haystack(feature) {
  return `${feature.dataset.terms || ""} ${feature.textContent}`.toLowerCase();
}

function applyFilter() {
  const query = filter.value.trim().toLowerCase();
  let shown = 0;
  for (const feature of features) {
    const hidden = query.length > 0 && !haystack(feature).includes(query);
    feature.classList.toggle("is-filtered", hidden);
    if (!hidden) shown += 1;
  }
  // A section whose every card is filtered out keeps only empty chrome, so its
  // heading goes with them.
  for (const section of sections) {
    const cards = section.querySelectorAll(".feature");
    const visible = section.querySelectorAll(".feature:not(.is-filtered)");
    section.hidden = query.length > 0 && cards.length > 0 && visible.length === 0;
  }
  // A contents entry points at either a card or a whole section; it survives
  // only while whatever it points at is still on the page.
  for (const link of links) {
    const target = document.getElementById(link.getAttribute("href").slice(1));
    const card = target?.closest(".feature");
    const section = target?.closest(".help-section");
    const gone = !target || card?.classList.contains("is-filtered") || section?.hidden;
    link.classList.toggle("is-filtered", Boolean(query.length > 0 && gone));
  }
  for (const group of document.querySelectorAll(".toc-group")) {
    const groupLinks = Array.from(group.querySelectorAll(".toc-link"));
    const anyVisible = groupLinks.some((link) => !link.classList.contains("is-filtered"));
    group.classList.toggle("is-filtered", groupLinks.length > 0 && !anyVisible);
  }
  empty.hidden = !(query.length > 0 && shown === 0);
}

filter?.addEventListener("input", applyFilter);
filter?.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    filter.value = "";
    applyFilter();
  }
});

// "/" focuses the filter, the one shortcut a reference page earns.
document.addEventListener("keydown", (event) => {
  if (event.key !== "/" || event.target === filter) return;
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) return;
  event.preventDefault();
  filter?.focus();
});

syncActiveLink();
