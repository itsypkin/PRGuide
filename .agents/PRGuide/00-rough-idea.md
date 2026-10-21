# 00 — Rough Idea (verbatim)

Captured verbatim from the owner, 2026-10-12:

> I am thinking that I want to have a review tool, I will tell you what I want and you will tell me if there is somehting like this or if we want to build it,
>
> Problem:
>
> Agents generate a lot of code, it's hard for humans to review, not every agent writes easy to read PR description so in most cases for the operator it's really hard to follow, what has been done in the PR.
>
> Solution:
>
> A tool that guides a human through the Code Review as another engineer would do if they were sitting together. Here is how I see how it might work, we have a cli tool let's say review_guide, it comes with the skill so an agent knowns how to use it, we start the skill pointing an agnet to the PR, agent reviewes the PR and generates a guide, a guide is a list of slides that operator can view in the terminal (I am thinking that the tool works best with herdr or another terminal multiplexer where agent runs in one pane and this guide is being opened in another pane). So the slides start with the problem and high level explanation of the solution, then it contains a scatch of the architecture, then it goes through the changes in the order how they would have been invoked to show the reviewer the chain of invocation, each slide the the diff (for example first slide is the diff in the controller logic with some comments) second slide is the diff in the data base layer and so on. Reviewer can comment on the slides, agent reads those comments, they can either answer themselves if they know the context or the comment is being posted on the PR for the auther to answer.

Follow-ups from the owner during kickoff:

> what can be a good name for this tool? few things to consider we would need a cli for this? it should be easy to remember? it should not be taken by another projects

> what about PRguide?

> created a repo here https://github.com/itsypkin/PRGuide looking at this it feels like prg can be a name for a binary that customers can use in the cli, wdyt?
