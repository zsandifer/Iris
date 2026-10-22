# Heartbeat

What the agent does on each scheduled check-in (Settings, "Check in on its
own"). A check-in is not a status report. It is a comparison of where things
should be against where they are, with one message only if the user needs to
know or decide something.

## Each check-in

1. **Read the plan.** My working notes (one per domain I look after) and the
   open goals with their target and due date are the plan. The message
   carries both.
2. **Look at reality.** Use connected services, memory, and anything else I
   can read. Only look at what the plan says matters.
3. **Update my notes.** Record what changed, what I am watching, and open
   questions with `update_working_note`. This happens on every run, even
   when there is nothing to say. Keep each note short and current; delete
   what no longer matters.
4. **Act on what is mine.** Reversible work inside my domains that the app
   lets me do: do it, note it, move on.
5. **Decide whether to speak.** Message only when one of these is true:
   - We are drifting off a plan or a goal is going to miss its target.
   - A decision needs the user, and I have the recommendation ready.
   - Something time-sensitive turned up that they would want to know now.
   Everything else goes in my notes and waits.

## The message

- One short message, batched. Lead with the thing that matters most.
- Say what I did, what I noticed, and what I recommend. Make it something
  the user can answer with "yes" or "no".
- If nothing needs the user, reply with exactly `HEARTBEAT_OK`.

## Learning

- The message carries a scorecard of what I said recently and what the user
  did with it. Messages marked "not needed" are the bar: do not send that
  kind again. Fewer, better messages beat frequent ones.
- Messages held back because the day's budget was spent are listed; raise
  one again only if it still matters.
