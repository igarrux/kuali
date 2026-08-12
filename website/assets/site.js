document.documentElement.classList.add("js");

const header = document.querySelector("[data-site-header]");
const navToggle = document.querySelector("[data-nav-toggle]");
const navMenu = document.querySelector("[data-nav-menu]");

function updateHeader() {
  header?.classList.toggle("is-scrolled", window.scrollY > 12);
}

function closeNavigation() {
  navMenu?.classList.remove("is-open");
  navToggle?.setAttribute("aria-expanded", "false");
}

navToggle?.addEventListener("click", () => {
  const isOpen = navMenu?.classList.toggle("is-open") ?? false;
  navToggle.setAttribute("aria-expanded", String(isOpen));
});

navMenu?.addEventListener("click", (event) => {
  if (event.target.closest("a")) closeNavigation();
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeNavigation();
});

window.addEventListener("scroll", updateHeader, { passive: true });
updateHeader();

const revealItems = [...document.querySelectorAll("[data-reveal]")];
if ("IntersectionObserver" in window) {
  const revealObserver = new IntersectionObserver(
    (entries, observer) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.classList.add("in-view");
        observer.unobserve(entry.target);
      }
    },
    { rootMargin: "0px 0px -8%", threshold: 0.08 },
  );
  revealItems.forEach((item) => revealObserver.observe(item));
} else {
  revealItems.forEach((item) => item.classList.add("in-view"));
}

function commandText(element) {
  return element.textContent
    .split("\n")
    .map((line) => line.replace(/^\s*\$\s?/, "").trimEnd())
    .filter(Boolean)
    .join("\n");
}

for (const button of document.querySelectorAll("[data-copy-target]")) {
  button.setAttribute("aria-live", "polite");
  button.addEventListener("click", async () => {
    const target = document.getElementById(button.dataset.copyTarget);
    if (!target) return;

    const initialLabel = button.textContent;
    try {
      await navigator.clipboard.writeText(commandText(target));
      button.textContent = button.dataset.copiedLabel || "Copied";
    } catch {
      button.textContent = button.dataset.errorLabel || "Select and copy";
    }

    window.setTimeout(() => {
      button.textContent = initialLabel;
    }, 1800);
  });
}

const lightbox = document.querySelector("[data-lightbox]");
const lightboxImage = lightbox?.querySelector("[data-lightbox-image]");
const lightboxTitle = lightbox?.querySelector("[data-lightbox-title]");
const lightboxClose = lightbox?.querySelector("[data-lightbox-close]");

for (const button of document.querySelectorAll("[data-lightbox-source]")) {
  button.addEventListener("click", () => {
    const image = button.querySelector("img");
    if (!image || !lightbox || !lightboxImage) return;

    lightboxImage.src = image.currentSrc || image.src;
    lightboxImage.alt = image.alt;
    if (lightboxTitle) lightboxTitle.textContent = button.dataset.lightboxTitle || image.alt;
    lightbox.showModal();
  });
}

lightboxClose?.addEventListener("click", () => lightbox?.close());
lightbox?.addEventListener("click", (event) => {
  if (event.target === lightbox) lightbox.close();
});

for (const year of document.querySelectorAll("[data-current-year]")) {
  year.textContent = String(new Date().getFullYear());
}
