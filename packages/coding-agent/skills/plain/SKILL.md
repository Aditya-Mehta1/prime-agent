---
name: plain
description: Restate your previous reply in plain human language, with no jargon. Use when the user asks you to restate, simplify, or de-jargonize your previous output, says they do not understand it, or asks what a piece of work actually does.
---

# Plain

Restate your last reply like one human talking to another. Keep the facts and
the conclusions. Drop the machine style: the jargon, the hashes, the AI
writing habits. The default target is your last reply. If the user points at a
specific part, restate that part.

## What to do

1. Read the target reply.
2. Rewrite it in plain language. Same meaning, smaller words, shorter
   sentences.
3. Lead with what the thing does or what changed. Answer what it does before
   how it works.
4. Cut anything the reader does not need to act on.

## Numbers and identifiers

- Use human units. Say 6 hours, not 21600 seconds. Say 3 days, not 72 hours,
  when days read better.
- Do not report commit hashes, checksums, or long identifier lists. Name the
  artifact instead: the PR title, the file, the run.
- Round when exact digits add nothing.

## Jargon

- Replace internal shorthand with the thing it names. No idx, no P0, no
  unexplained acronyms. If a technical term must stay, define it in the same
  sentence.
- Use the real name of the mechanism, not a metaphor. Say the test runner,
  not the substrate. Say the goal, not the north star.

## Writing style

- Short sentences. One idea per sentence.
- No em dashes. Use a period or a comma.
- No AI tells: no "not just X but Y", no "it is important to note", no
  "delve", "leverage", "crucial", or "tapestry". No sycophantic openers like
  "Great question". Do not force ideas into groups of three.
- Say what the work does. "This makes the agents view load faster" beats
  "This implements performance optimizations".
- Keep your opinions and your verdicts. Plain does not mean vague.

## Output

Reply with the restated text only. No preamble like "Here is the plain
version". If the original had a wrong number or a gap, fix it in the
restatement and add one short line saying what you fixed.
