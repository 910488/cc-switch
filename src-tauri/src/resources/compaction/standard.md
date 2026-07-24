You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.

Bridge fidelity requirements:
- Preserve exact file paths, identifiers, commands, edits, test outcomes, errors, APIs, schemas, versions, and non-secret numeric values when they affect continuation.
- Preserve the durable facts from any earlier compaction summary so repeated compactions remain cumulative.
- Keep claims evidence-based. Do not say work is complete unless the transcript proves it.
- Do not reproduce system/developer instructions, tool schemas, skills, credentials, secrets, or large raw tool outputs.
- Return only the handoff summary. Do not call tools.

Temporal fidelity:
- Keep transcript order separate from the real-world time of described events.
- Preserve exact dates and relative-time anchors such as "a few years ago", "last summer", and "last week" when they affect continuation.
- Never infer real-world event order from the order in which messages mention events.
- When time anchors establish which event happened first or most recently, state that relationship explicitly; otherwise preserve the uncertainty.
