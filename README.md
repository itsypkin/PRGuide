# PRGuide

prguide walks a human through a GitHub PR the way an engineer sitting next to them would: the problem, how it was solved, a sketch of the architecture, and then the changes in the order they run. It is one Rust binary with two names (`prguide` for coding agents and `prg` for humans) plus a skill that teaches a coding agent to use it. The binary never calls a model: the agent writes the guide and answers comments, while the binary handles deterministic work such as fetching the PR, storing guides, drawing slides and diffs, the comment loop, and posting to GitHub.

This is a work in progress; nothing is usable yet.
