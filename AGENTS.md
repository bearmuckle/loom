# Agent instructions

- Keep PR verification sections terse: list each verification command once, with a brief pass or fail result. Do not include verbose test output.
- For UI work, prefer existing `gpui-kit` components when they fit the interaction; build custom controls only when the component library does not provide a suitable option.

# Commit attribution

When creating commits in this repository, attribute the commit to the repository
owner's GitHub account so deployment providers can match the commit to its
author:

- Name: `Björn Harrtell`
- Email: `141030+bjornharrtell@users.noreply.github.com`

Before committing, set these values in the repository-local Git config and
verify them with `git var GIT_AUTHOR_IDENT`. Do not use the generic Copilot
identity (`198982749+Copilot@users.noreply.github.com`) as the commit author.
