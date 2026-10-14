#!/usr/bin/env bash
# Leaves one comment on the pull request showing, before and after, each screen
# that looks different from the base branch.
#
# Only the changed shots are pushed, to an orphan branch, because a comment
# cannot carry an image directly — it has to already be reachable by URL.
# Keeping the branch orphaned means these never appear in the history of
# anything that ships.
set -euo pipefail

BRANCH=screenshots
DIR="pr-${PR}/${SHA:0:7}"
REPO="${GITHUB_REPOSITORY}"
SHOTS="${GITHUB_WORKSPACE}/shots/changed"
CHANGED="${SHOTS}/changed.json"
count=$(jq '.changed | length' "$CHANGED")
total=$(jq '.total' "$CHANGED")
baseline=$(jq '.baseline' "$CHANGED")

marker="## Screenshots"
existing=$(gh api "repos/${REPO}/issues/${PR}/comments" --paginate --jq \
  ".[] | select(.user.login == \"github-actions[bot]\") | select(.body | startswith(\"${marker}\")) | .id" | sed -n 1p)

# Every pull request pushes to this one branch, and the concurrency group is
# per pull request, so two runs can race. Each attempt starts from the branch as
# it stands now, and a rejected push just goes round again on the newer tip.
publish() {
  rm -rf /tmp/shots-branch
  git worktree prune
  # Take the branch as it stands, or start one with no history behind it.
  if git ls-remote --exit-code --heads origin "$BRANCH" >/dev/null 2>&1; then
    git fetch origin "$BRANCH" --depth 1
    git worktree add --detach /tmp/shots-branch "origin/$BRANCH"
    git -C /tmp/shots-branch switch -C "$BRANCH"
  else
    git worktree add --detach /tmp/shots-branch
    git -C /tmp/shots-branch checkout --orphan "$BRANCH"
    git -C /tmp/shots-branch rm -rf . >/dev/null 2>&1 || true
  fi

  mkdir -p "/tmp/shots-branch/${DIR}"
  cp "${SHOTS}"/*.png "/tmp/shots-branch/${DIR}/"

  git -C /tmp/shots-branch add -A
  if git -C /tmp/shots-branch diff --cached --quiet; then
    echo "screenshots unchanged; nothing to push"
    return 0
  fi
  git -C /tmp/shots-branch commit -q -m "Screenshots for #${PR} at ${SHA:0:7}"
  git -C /tmp/shots-branch push -q origin "$BRANCH"
}

raw() { echo "https://raw.githubusercontent.com/${REPO}/${BRANCH}/${DIR}/$1"; }

if [ "$count" = 0 ]; then
  # Nothing to show. Only touch an earlier comment, so shots that no longer
  # match the branch do not linger; never start a new one to say so.
  if [ -z "$existing" ]; then
    echo "no screens changed; nothing to comment"
    exit 0
  fi
  {
    echo "$marker"
    echo
    echo "No screen looks different from \`${BASE_REF}\` as of \`${SHA:0:7}\`."
  } > /tmp/comment.md
else
  git config user.name "github-actions[bot]"
  git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
  for attempt in 1 2 3 4 5; do
    if publish; then break; fi
    if [ "$attempt" = 5 ]; then
      echo "could not push ${BRANCH} after ${attempt} attempts" >&2
      exit 1
    fi
    echo "push to ${BRANCH} was rejected (attempt ${attempt}); retrying on the newer tip"
    sleep $((attempt * 2))
  done

  {
    echo "$marker"
    echo
    if [ "$baseline" = true ]; then
      echo "${count} of ${total} screens look different from \`${BASE_REF}\` as of \`${SHA:0:7}\`."
    else
      echo "\`${BASE_REF}\` could not be captured to compare against, so all ${total} screens are shown as of \`${SHA:0:7}\`."
    fi
    echo
    echo "<sub>The harness stubs the printer status (and, on some screens, events and settings);"
    echo "the page and its script are the real ones. Notification support is stood in for on the settings shots, because"
    echo "headless Chromium refuses notifications outright.</sub>"
    echo
    jq -c '.changed[]' "$CHANGED" | while read -r shot; do
      name=$(jq -r '.name' <<<"$shot")
      title=$(jq -r '.title' <<<"$shot")
      note=$(jq -r '.note' <<<"$shot")
      reason=$(jq -r '.reason' <<<"$shot")
      echo "### ${title}$([ "$reason" = new ] && echo " (new)")"
      echo
      echo "$note"
      echo
      if [ -f "${SHOTS}/${name}.base.png" ]; then
        echo "| Before | After |"
        echo "| --- | --- |"
        echo "| <img src=\"$(raw "${name}.base.png")\" width=\"320\"> | <img src=\"$(raw "${name}.png")\" width=\"320\"> |"
      else
        echo "<img src=\"$(raw "${name}.png")\" width=\"320\">"
      fi
      echo
    done
  } > /tmp/comment.md
fi

# One comment per pull request, edited in place, so a run of pushes does not
# bury the conversation.
if [ -n "$existing" ]; then
  gh api -X PATCH "repos/${REPO}/issues/comments/${existing}" -F body=@/tmp/comment.md >/dev/null
  echo "updated comment ${existing}"
else
  gh api -X POST "repos/${REPO}/issues/${PR}/comments" -F body=@/tmp/comment.md >/dev/null
  echo "posted a new comment"
fi
