# Playlist manager

This thing sits somewhere in background, listening on a unix domain socket. You
can load it with a playlist and it'll keep executing mpv to play it. You can
replace the playlist, send it commands to switch to other songs, etc…

Basically, I was unhappy about all the music players, because they play around
with collections, try to work with albums derived from ID3 tags (which are
usually wrong) instead of file system paths and just make everything bloated and
complicated. I simply wanted to play random songs from a big bag.
