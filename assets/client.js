document.documentElement.classList.add("js-enabled");

const settingsSearch = document.querySelector('#settings-search');
if (settingsSearch) {
  settingsSearch.closest(".settings-search").hidden = false;
  settingsSearch.addEventListener("input", () => {
    const query = settingsSearch.value.trim().toLocaleLowerCase();
    let count = 0;
    document.querySelectorAll(".deep-settings-field").forEach((field) => {
      field.hidden = !field.textContent.toLocaleLowerCase().includes(query);
      if (!field.hidden) count += 1;
    });
    document.querySelectorAll(".deep-settings-group").forEach((group) => {
      group.hidden = !Array.from(group.querySelectorAll(".deep-settings-field")).some((field) => !field.hidden);
    });
    document.querySelector('#settings-search-status').textContent = `${count} settings found`;
  });
  document.querySelectorAll(".settings-category-nav a").forEach((link) => {
    link.addEventListener("click", () => {
      settingsSearch.value = "";
      settingsSearch.dispatchEvent(new Event("input"));
    });
  });
}

function cardInteractiveTarget(target) {
  if (!(target instanceof Element)) {
    return null;
  }
  return target.closest('a,button,input,textarea,select,label,form,[role="button"],[data-youtube-preview]');
}

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const formCard = event.target.closest("[data-card-form]");
  if (formCard && !cardInteractiveTarget(event.target)) {
    const form = document.getElementById(formCard.getAttribute("data-card-form"));
    if (form instanceof HTMLFormElement) {
      event.preventDefault();
      if (form.requestSubmit) {
        form.requestSubmit();
      } else {
        form.submit();
      }
      return;
    }
  }
  const card = event.target.closest("[data-card-href]");
  if (!card || cardInteractiveTarget(event.target)) {
    return;
  }
  window.location.assign(card.getAttribute("data-card-href"));
});

function safeYoutubeEmbedSrc(value) {
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.hostname !== "www.youtube-nocookie.com" || !url.pathname.startsWith("/embed/")) {
      return null;
    }
    url.searchParams.set("autoplay", "1");
    url.searchParams.set("rel", "0");
    return url.toString();
  } catch (_error) {
    return null;
  }
}

function activateYoutubePreview(link) {
  const card = link.closest("[data-youtube-preview]");
  const player = card ? card.querySelector("[data-youtube-player]") : null;
  if (!card || !player || player.querySelector("iframe")) {
    return;
  }
  const src = safeYoutubeEmbedSrc(link.getAttribute("data-youtube-embed-src") || "");
  if (!src) {
    return;
  }
  const iframe = document.createElement("iframe");
  iframe.className = "youtube-iframe";
  iframe.src = src;
  iframe.title = link.getAttribute("data-youtube-title") || "YouTube video";
  iframe.loading = "lazy";
  iframe.referrerPolicy = "no-referrer";
  iframe.allow = "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share";
  iframe.setAttribute("allowfullscreen", "");
  iframe.setAttribute("sandbox", "allow-scripts allow-same-origin allow-presentation");
  player.hidden = false;
  player.append(iframe);
  card.classList.add("youtube-preview-playing");
}

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const link = event.target.closest("[data-youtube-play]");
  if (!link) {
    return;
  }
  event.preventDefault();
  activateYoutubePreview(link);
});

document.addEventListener("keydown", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  if (event.key === "Enter" && event.target === event.target.closest("[data-card-form]")) {
    const form = document.getElementById(event.target.getAttribute("data-card-form"));
    if (form instanceof HTMLFormElement) {
      event.preventDefault();
      if (form.requestSubmit) {
        form.requestSubmit();
      } else {
        form.submit();
      }
      return;
    }
  }
  if (event.key !== "Enter" || event.target !== event.target.closest("[data-card-href]")) {
    return;
  }
  window.location.assign(event.target.getAttribute("data-card-href"));
});

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const backLink = event.target.closest("[data-history-back]");
  if (!backLink || window.history.length <= 1) {
    return;
  }
  event.preventDefault();
  window.history.back();
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-password-toggle]");
  if (!button) {
    return;
  }
  const input = document.getElementById(button.getAttribute("data-password-toggle"));
  if (!input) {
    return;
  }
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  button.textContent = show ? "Hide" : "Show";
  button.setAttribute("aria-label", show ? "Hide password" : "Show password");
});

function closeRepostMenus(except, restoreFocus) {
  document.querySelectorAll("[data-repost-menu]").forEach((menu) => {
    if (menu === except) {
      return;
    }
    const button = document.querySelector(`[aria-controls="${menu.id}"]`);
    const shouldRestoreFocus = restoreFocus && menu.contains(document.activeElement);
    menu.hidden = true;
    if (button) {
      button.setAttribute("aria-expanded", "false");
      if (shouldRestoreFocus) {
        button.focus();
      }
    }
  });
}

function openRepostMenu(button) {
  const menu = document.getElementById(button.getAttribute("aria-controls"));
  if (!menu) {
    return;
  }
  closeRepostMenus(menu, false);
  menu.hidden = false;
  button.setAttribute("aria-expanded", "true");
  const firstItem = menu.querySelector('[role="menuitem"]');
  if (firstItem) {
    firstItem.focus();
  }
}

document.addEventListener("pointerdown", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (!button) {
    return;
  }
  const timer = window.setTimeout(() => {
    button.dataset.longPressOpen = "true";
    openRepostMenu(button);
  }, 450);
  button.dataset.longPressTimer = String(timer);
});

document.addEventListener("pointerup", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (!button || !button.dataset.longPressTimer) {
    return;
  }
  window.clearTimeout(Number.parseInt(button.dataset.longPressTimer, 10));
  delete button.dataset.longPressTimer;
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (button && button.dataset.longPressOpen === "true") {
    event.preventDefault();
    event.stopPropagation();
    delete button.dataset.longPressOpen;
    return;
  }
  if (!event.target.closest("[data-repost-control]")) {
    closeRepostMenus(null, false);
  }
});

document.addEventListener("keydown", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (button && (event.key === "ArrowDown" || event.key === "F10")) {
    event.preventDefault();
    openRepostMenu(button);
    return;
  }
  if (event.key === "Escape") {
    closeRepostMenus(null, true);
  }
});

const CHARACTER_COUNTER_CLASSES = [
  "character-counter-normal",
  "character-counter-warning",
  "character-counter-danger"
];

function characterCounterFor(textarea) {
  const counter = document.querySelector(`[data-character-counter="${textarea.id}"]`);
  return counter;
}

function characterCounterClass(remaining) {
  if (remaining <= 10) {
    return "character-counter-danger";
  }
  if (remaining <= 50) {
    return "character-counter-warning";
  }
  return "character-counter-normal";
}

function updateComposerSubmit(textarea, remaining) {
  const form = textarea.closest("form");
  const submit = form ? form.querySelector('button[type="submit"]') : null;
  if (!submit) {
    return;
  }
  submit.disabled = remaining < 0 || form.dataset.submitting === "true";
}

function updateComposerCount(textarea) {
  const counter = characterCounterFor(textarea);
  if (!counter) {
    return;
  }
  const max = Number.parseInt(textarea.getAttribute("data-character-limit") || "0", 10);
  if (!Number.isFinite(max)) {
    return;
  }
  const length = Array.from(textarea.value).length;
  const remaining = max - length;
  counter.textContent = `${remaining} remaining`;
  counter.classList.remove(...CHARACTER_COUNTER_CLASSES);
  counter.classList.add(characterCounterClass(remaining));
  updateComposerSubmit(textarea, remaining);
}

function initializeComposerCounters(root) {
  if (root.matches && root.matches("textarea[data-character-limit]")) {
    updateComposerCount(root);
  }
  if (root.querySelectorAll) {
    root.querySelectorAll("textarea[data-character-limit]").forEach(updateComposerCount);
  }
}

const mentionStates = new WeakMap();
const MENTION_DEBOUNCE_MS = 120;

function mentionStateFor(textarea) {
  let state = mentionStates.get(textarea);
  if (state) {
    return state;
  }
  const menu = document.getElementById(textarea.getAttribute("aria-controls"));
  if (!menu) {
    return null;
  }
  state = {
    textarea,
    menu,
    items: [],
    activeIndex: -1,
    atIndex: -1,
    fragment: "",
    requestId: 0,
    timer: 0,
    controller: null
  };
  mentionStates.set(textarea, state);
  return state;
}

function mentionContext(textarea) {
  if (textarea.selectionStart !== textarea.selectionEnd) {
    return null;
  }
  const cursor = textarea.selectionStart;
  const before = textarea.value.slice(0, cursor);
  const atIndex = before.lastIndexOf("@");
  if (atIndex < 0) {
    return null;
  }
  const preceding = atIndex > 0 ? before.charAt(atIndex - 1) : "";
  if (preceding && /[A-Za-z0-9_@.-]/.test(preceding)) {
    return null;
  }
  const fragment = before.slice(atIndex + 1);
  if (!/^[A-Za-z0-9_-]*$/.test(fragment)) {
    return null;
  }
  return { atIndex, fragment };
}

function closeMentionMenu(textarea) {
  const state = mentionStateFor(textarea);
  if (!state) {
    return;
  }
  if (state.timer) {
    window.clearTimeout(state.timer);
    state.timer = 0;
  }
  if (state.controller) {
    state.controller.abort();
    state.controller = null;
  }
  state.items = [];
  state.activeIndex = -1;
  state.atIndex = -1;
  state.fragment = "";
  state.menu.hidden = true;
  state.menu.textContent = "";
  textarea.setAttribute("aria-expanded", "false");
  textarea.removeAttribute("aria-activedescendant");
}

function setActiveMention(state, index) {
  if (state.items.length === 0) {
    state.activeIndex = -1;
    state.textarea.removeAttribute("aria-activedescendant");
    return;
  }
  state.activeIndex = (index + state.items.length) % state.items.length;
  state.menu.querySelectorAll("[role='option']").forEach((option, optionIndex) => {
    const selected = optionIndex === state.activeIndex;
    option.setAttribute("aria-selected", selected ? "true" : "false");
    if (selected) {
      state.textarea.setAttribute("aria-activedescendant", option.id);
      option.scrollIntoView({ block: "nearest" });
    }
  });
}

function selectMention(textarea, index) {
  const state = mentionStateFor(textarea);
  if (!state || index < 0 || index >= state.items.length) {
    return;
  }
  const context = mentionContext(textarea);
  if (!context || context.atIndex !== state.atIndex) {
    closeMentionMenu(textarea);
    return;
  }
  const username = state.items[index].username;
  const start = state.atIndex;
  const end = textarea.selectionStart;
  const suffix = textarea.value.slice(end);
  const needsSpace = suffix.length === 0 || /^[A-Za-z0-9_@#-]/.test(suffix);
  const replacement = `@${username}${needsSpace ? " " : ""}`;
  textarea.value = `${textarea.value.slice(0, start)}${replacement}${suffix}`;
  const cursor = start + replacement.length;
  textarea.setSelectionRange(cursor, cursor);
  closeMentionMenu(textarea);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

function renderMentionSuggestions(state, suggestions) {
  state.items = suggestions;
  state.menu.textContent = "";
  if (suggestions.length === 0) {
    closeMentionMenu(state.textarea);
    return;
  }
  suggestions.forEach((suggestion, index) => {
    const option = document.createElement("button");
    option.type = "button";
    option.className = "mention-option";
    option.id = `${state.menu.id}-option-${index}`;
    option.setAttribute("role", "option");
    option.setAttribute("aria-selected", "false");
    option.dataset.username = suggestion.username;
    const name = document.createElement("span");
    name.className = "mention-name";
    name.textContent = suggestion.display_name || suggestion.username;
    const handle = document.createElement("span");
    handle.className = "mention-handle";
    handle.textContent = `@${suggestion.username}`;
    option.append(name, handle);
    option.addEventListener("pointerdown", (event) => {
      event.preventDefault();
      selectMention(state.textarea, index);
    });
    option.addEventListener("mouseenter", () => setActiveMention(state, index));
    state.menu.append(option);
  });
  state.menu.hidden = false;
  state.textarea.setAttribute("aria-expanded", "true");
  setActiveMention(state, 0);
}

function requestMentionSuggestions(textarea) {
  const state = mentionStateFor(textarea);
  const context = mentionContext(textarea);
  if (!state || !context || !window.fetch) {
    closeMentionMenu(textarea);
    return;
  }
  state.atIndex = context.atIndex;
  state.fragment = context.fragment;
  if (state.timer) {
    window.clearTimeout(state.timer);
  }
  state.timer = window.setTimeout(async () => {
    state.timer = 0;
    if (state.controller) {
      state.controller.abort();
    }
    const requestId = state.requestId + 1;
    state.requestId = requestId;
    state.controller = window.AbortController ? new AbortController() : null;
    try {
      const response = await fetch(`/mentions?q=${encodeURIComponent(context.fragment)}`, {
        headers: { "Accept": "application/json" },
        credentials: "same-origin",
        signal: state.controller ? state.controller.signal : undefined
      });
      if (!response.ok || requestId !== state.requestId) {
        return;
      }
      const suggestions = await response.json();
      const current = mentionContext(textarea);
      if (!current || current.atIndex !== context.atIndex || current.fragment !== context.fragment) {
        return;
      }
      renderMentionSuggestions(state, Array.isArray(suggestions) ? suggestions : []);
    } catch (error) {
      if (!state.controller || error.name !== "AbortError") {
        closeMentionMenu(textarea);
      }
    }
  }, MENTION_DEBOUNCE_MS);
}

function initializeMentionAutocomplete(root) {
  if (root.matches && root.matches("textarea[data-mention-autocomplete]")) {
    mentionStateFor(root);
  }
  if (root.querySelectorAll) {
    root.querySelectorAll("textarea[data-mention-autocomplete]").forEach(mentionStateFor);
  }
}

document.addEventListener("input", (event) => {
  const textarea = event.target.closest("textarea[data-character-limit]");
  if (textarea) {
    updateComposerCount(textarea);
  }
  const mentionTextarea = event.target.closest("textarea[data-mention-autocomplete]");
  if (mentionTextarea) {
    requestMentionSuggestions(mentionTextarea);
  }
});

document.addEventListener("keydown", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const textarea = event.target.closest("textarea[data-mention-autocomplete]");
  if (!textarea) {
    return;
  }
  const state = mentionStateFor(textarea);
  if (!state || state.menu.hidden) {
    return;
  }
  if (event.key === "ArrowDown") {
    event.preventDefault();
    setActiveMention(state, state.activeIndex + 1);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    setActiveMention(state, state.activeIndex - 1);
  } else if (event.key === "Enter" || event.key === "Tab") {
    event.preventDefault();
    selectMention(textarea, state.activeIndex);
  } else if (event.key === "Escape") {
    event.preventDefault();
    closeMentionMenu(textarea);
  }
});

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  document.querySelectorAll("textarea[data-mention-autocomplete]").forEach((textarea) => {
    const state = mentionStateFor(textarea);
    if (state && event.target !== textarea && !state.menu.contains(event.target)) {
      closeMentionMenu(textarea);
    }
  });
});

initializeComposerCounters(document);
initializeMentionAutocomplete(document);
window.addEventListener("pageshow", () => initializeComposerCounters(document));
window.requestAnimationFrame(() => initializeComposerCounters(document));
if (window.MutationObserver) {
  new MutationObserver((mutations) => {
    mutations.forEach((mutation) => {
      mutation.addedNodes.forEach((node) => {
        initializeComposerCounters(node);
        initializeMentionAutocomplete(node);
      });
    });
  }).observe(document.documentElement, { childList: true, subtree: true });
}

function mediaSummary(input) {
  if (!input.files || input.files.length === 0) {
    return "";
  }
  if (input.files.length === 1) {
    return input.files[0].name || "1 media file selected";
  }
  return `${input.files.length} media files selected`;
}

function updateComposerMedia(input) {
  const form = input.closest("form");
  if (!form) {
    return;
  }
  const hasMedia = !!(input.files && input.files.length > 0);
  const selection = form.querySelector("[data-composer-media-selection]");
  const summary = form.querySelector("[data-composer-media-summary]");
  const nsfw = form.querySelector("[data-composer-nsfw]");
  if (summary) {
    summary.textContent = mediaSummary(input);
  }
  if (selection) {
    selection.hidden = !hasMedia;
  }
  if (nsfw && !hasMedia) {
    nsfw.checked = false;
  }
}

document.querySelectorAll("input[type=file][data-composer-media]").forEach((input) => {
  updateComposerMedia(input);
  input.addEventListener("change", () => updateComposerMedia(input));
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-composer-clear-media]");
  if (!button) {
    return;
  }
  const form = button.closest("form");
  const input = form ? form.querySelector("input[type=file][data-composer-media]") : null;
  if (!input) {
    return;
  }
  input.value = "";
  updateComposerMedia(input);
  input.focus();
});

function mediaFileName(input) {
  if (!input.files || input.files.length === 0) {
    return "";
  }
  return input.files[0].name || "media file";
}

function setProfileMediaStatus(frame, message) {
  const status = frame.querySelector("[data-profile-media-status]");
  if (status) {
    status.textContent = message;
  }
}

function updateProfileMediaFile(input) {
  const frame = input.closest("[data-profile-media-frame]");
  if (!frame) {
    return;
  }
  const fileName = mediaFileName(input);
  frame.classList.toggle("settings-media-has-file", fileName !== "");
  if (fileName !== "") {
    const remove = frame.querySelector("[data-profile-media-delete]");
    if (remove) {
      remove.checked = false;
      frame.classList.remove("settings-media-removing");
    }
    setProfileMediaStatus(frame, `${fileName} selected`);
  } else {
    setProfileMediaStatus(frame, "");
  }
}

function updateProfileMediaDelete(input) {
  const frame = input.closest("[data-profile-media-frame]");
  if (!frame) {
    return;
  }
  frame.classList.toggle("settings-media-removing", input.checked);
  const file = frame.querySelector("[data-profile-media-file]");
  if (input.checked && file) {
    file.value = "";
    updateProfileMediaFile(file);
    frame.classList.add("settings-media-removing");
  }
  const label = input.getAttribute("aria-label") || "Media";
  setProfileMediaStatus(frame, input.checked ? `${label} selected` : `${label} cancelled`);
}

document.querySelectorAll("[data-profile-media-file]").forEach(updateProfileMediaFile);

document.addEventListener("change", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const file = event.target.closest("[data-profile-media-file]");
  if (file) {
    updateProfileMediaFile(file);
    return;
  }
  const remove = event.target.closest("[data-profile-media-delete]");
  if (remove) {
    updateProfileMediaDelete(remove);
  }
});

function resetSubmittingForms() {
  document.querySelectorAll('form[data-submitting="true"]').forEach((form) => {
    delete form.dataset.submitting;
    form.querySelectorAll("button:disabled").forEach((button) => {
      button.disabled = false;
    });
  });
}

window.addEventListener("pageshow", (event) => {
  const nav = performance.getEntriesByType("navigation")[0];
  const restored = event.persisted || (nav && nav.type === "back_forward");
  if (restored) {
    resetSubmittingForms();
  }
});

function setButtonState(button, active, label) {
  button.classList.toggle("active", active);
  button.setAttribute("aria-pressed", active ? "true" : "false");
  button.setAttribute("aria-label", label);
  button.setAttribute("title", label);
  const text = button.querySelector("[data-button-label]");
  if (text) {
    text.textContent = label;
  }
}

function clearFormError(form) {
  const existing = form.querySelector("[data-form-error]");
  if (existing) {
    existing.remove();
  }
}

function showFormError(form, message) {
  clearFormError(form);
  const notice = document.createElement("p");
  notice.className = "notice error";
  notice.setAttribute("role", "alert");
  notice.setAttribute("data-form-error", "");
  notice.textContent = message;
  form.prepend(notice);
}

function enhancedErrorMessage(status) {
  if (status === 503) {
    return "The server is busy. Please wait a moment and try again.";
  }
  if (status === 429) {
    return "Too many requests. Please wait a moment and try again.";
  }
  if (status === 400 || status === 422) {
    return "That submission was rejected. Check the text and any attached media, then try again.";
  }
  if (status === 401 || status === 403) {
    return "That action was refused. Your sign-in may have changed or expired. Save any unsaved text before reloading.";
  }
  return "Something went wrong. Please try again.";
}

document.addEventListener("submit", async (event) => {
  const form = event.target.closest("form[data-enhance]");
  if (!form || !window.fetch) {
    return;
  }
  event.preventDefault();
  if (form.dataset.submitting === "true") {
    return;
  }
  form.dataset.submitting = "true";
  form.setAttribute("aria-busy", "true");
  const submitter = event.submitter || form.querySelector("button[type=submit]");
  if (submitter) {
    submitter.disabled = true;
  }
  try {
    const formData = new FormData(form);
    const body = form.enctype === "multipart/form-data" ? formData : new URLSearchParams(formData);
    const response = await fetch(form.action, {
      method: form.method || "POST",
      body,
      headers: { "Accept": "application/json", "X-RustPost-Enhance": "1" },
      credentials: "same-origin"
    });
    if (!response.ok) {
      showFormError(form, enhancedErrorMessage(response.status));
      return;
    }
    clearFormError(form);
    const data = await response.json();
    if (data.kind === "follow") {
      const followForm = document.querySelector(`[data-follow-user="${data.user_id}"]`);
      if (followForm) {
        followForm.action = data.action;
        const button = followForm.querySelector("button");
        if (button) {
          const label = data.following ? "Following" : (data.requested ? "Requested" : "Follow");
          const aria = data.following ? "Unfollow this account" : (data.requested ? "Cancel follow request" : "Follow this account");
          button.classList.toggle("active", data.following);
          button.textContent = label;
          button.setAttribute("aria-pressed", data.following ? "true" : "false");
          button.setAttribute("aria-label", aria);
          button.setAttribute("title", aria);
        }
      }
      document.querySelectorAll(`[data-profile-followers="${data.user_id}"]`).forEach((node) => {
        node.textContent = `${data.followers} ${data.followers === 1 ? "follower" : "followers"}`;
      });
      document.querySelectorAll(`[data-profile-following="${data.user_id}"]`).forEach((node) => {
        node.textContent = `${data.following_count} following`;
      });
    } else if (data.kind === "post-action") {
      document.querySelectorAll(`[data-post-id="${data.post_id}"]`).forEach((post) => {
        const likes = post.querySelector('[data-count="likes"]');
        const reposts = post.querySelector('[data-count="reposts"]');
        if (likes) {
          likes.textContent = `${data.likes} ${data.likes === 1 ? "like" : "likes"}`;
        }
        if (reposts) {
          reposts.textContent = `${data.reposts} ${data.reposts === 1 ? "repost" : "reposts"}`;
        }
        const liked = post.querySelector('[data-action-kind="like"]');
        const bookmarked = post.querySelector('[data-action-kind="bookmark"]');
        const reposted = post.querySelector('[data-action-kind="repost"]');
        if (liked) {
          setButtonState(liked, data.liked, data.liked ? "Unlike" : "Like");
        }
        if (bookmarked) {
          setButtonState(bookmarked, data.bookmarked, data.bookmarked ? "Unbookmark" : "Bookmark");
        }
        if (reposted) {
          setButtonState(reposted, data.reposted, data.reposted ? "Unrepost" : "Repost");
        }
      });
    } else if (data.kind === "post-created") {
      let timeline = document.querySelector(".timeline");
      const empty = document.querySelector(".empty-state");
      if (!timeline && empty) {
        empty.outerHTML = '<section class="timeline" aria-label="Posts"></section>';
        timeline = document.querySelector(".timeline");
      }
      if (!timeline) {
        window.location.assign(data.redirect);
        return;
      }
      timeline.insertAdjacentHTML(data.parent_post_id === null ? "afterbegin" : "beforeend", data.html);
      if (data.parent_post_id !== null) {
        document.querySelectorAll(`[data-post-id="${data.parent_post_id}"] [data-count="replies"]`).forEach((node) => {
          const current = Number.parseInt(node.textContent || "0", 10);
          const next = Number.isFinite(current) ? current + 1 : 1;
          node.textContent = `${next} ${next === 1 ? "reply" : "replies"}`;
        });
      }
      const created = document.getElementById(`post-${data.post_id}`);
      if (created) {
        created.setAttribute("tabindex", "-1");
        created.focus({ preventScroll: true });
      }
      if (window.history && data.redirect) {
        window.history.pushState(null, "", data.redirect);
      }
      form.reset();
      form.querySelectorAll("textarea[data-character-limit]").forEach(updateComposerCount);
      form.querySelectorAll("input[type=file][data-composer-media]").forEach(updateComposerMedia);
    }
  } catch (_err) {
    // Do not silently re-POST: the first request may have been committed even
    // though the response was lost.
    showFormError(
      form,
      "Network problem. Reload the page to check whether your submission was saved before trying again."
    );
  } finally {
    delete form.dataset.submitting;
    form.removeAttribute("aria-busy");
    if (submitter) {
      submitter.disabled = false;
    }
  }
});

document.addEventListener("submit", (event) => {
  const form = event.target.closest("form");
  if (!form || form.dataset.enhance) {
    return;
  }
  if ((form.method || "GET").toUpperCase() !== "POST") {
    return;
  }
  if (form.dataset.submitting === "true") {
    event.preventDefault();
    return;
  }
  form.dataset.submitting = "true";
  const submitter = event.submitter || form.querySelector("button[type=submit]");
  if (submitter) {
    submitter.disabled = true;
  }
});

async function copyTextToClipboard(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch (_err) {
      // Fall back to the selection-based path below for browsers that expose
      // the Clipboard API but deny writes in this context.
    }
  }
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "-1000px";
  document.body.append(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) {
    throw new Error("copy command failed");
  }
}

function setCopyButtonLabel(button, label) {
  const feedback = button.querySelector("[data-copy-feedback]");
  if (feedback) {
    feedback.textContent = label;
    return;
  }
  button.textContent = label;
}

document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy-text]");
  if (!button) {
    return;
  }
  event.preventDefault();
  event.stopPropagation();
  const original = button.getAttribute("data-copy-label") || button.textContent || "Copy";
  if (button.dataset.copyTimer) {
    window.clearTimeout(Number.parseInt(button.dataset.copyTimer, 10));
    delete button.dataset.copyTimer;
  }
  try {
    await copyTextToClipboard(button.getAttribute("data-copy-text") || "");
    setCopyButtonLabel(button, button.getAttribute("data-copied-label") || "Copied");
  } catch (_err) {
    setCopyButtonLabel(button, button.getAttribute("data-copy-failed-label") || "Failed");
  }
  button.dataset.copyTimer = String(window.setTimeout(() => {
    setCopyButtonLabel(button, original);
    delete button.dataset.copyTimer;
  }, 1600));
});