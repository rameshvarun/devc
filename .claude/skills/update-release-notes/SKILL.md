---
name: update-release-notes
description: Generate and publish GitHub release notes for a given release tag by diffing it against the previous release. Use when the user runs /update-release-notes <version> (e.g. "/update-release-notes 0.5.0") or asks to write/update release notes for a tagged release.
---

# Update Release Notes

Generate release notes for a release tag by comparing it to the previous release tag, then publish them to the GitHub release with `gh`.

## Input

The target version is passed as an argument, e.g. `/update-release-notes 0.5.0` or `/update-release-notes v0.5.0`. If no argument is given, ask the user which version.

## Steps

1. **Gather changes** between the previous tag and the target tag, looking at commit messages and source code diffs.
2. **Write the release notes.** Produce clear, user-facing Markdown.
3. Ask the user to approve the release notes, and handle any revisions.
4. **Publish with `gh`.** Update the GitHub release notes for the target tag.

## Release Notes Style Guide:
- Ignore README changes.
- One bullet point per item.
- Terse, yet fully informative.
- A single `# Release Notes` heading at the top, with no other headers or sections.