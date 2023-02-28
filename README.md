# LoL-SoloQ-Leaderboard
A simple Discord Bot to create a leaderboard in a given channel

This was a day project I made on a whim. Most of the code is probably terrible and could use a lot of improvement, but I cba fixing it.

If you want to run it, update the values in `.env.local` and change the name to `.env`

There are two commands:

- leaderboard
  - It makes the current channel the leaderboard channel and prevents more messages from being sent.
- register
  - It takes a summoner name and ensures it's valid. The summoner ID is saved to the DB in case of name changes in the future.
