#modern-config-incompatible

#require no-eden


Setup

  $ configure dummyssh
  $ setconfig ui.username="nobody <no.reply@fb.com>"

Setup pushrebase required repo

  $ hg init server
  $ cd server
  $ enable pushrebase
  $ setconfig pushrebase.blocknonpushrebase=true
  $ echo a > a && hg commit -Aqm a
  $ cd ..

  $ hg clone -q server client
  $ cd client
  $ echo b >> a && hg commit -Aqm b
  $ hg book master

Non-pushrebase pushes should be rejected

  $ hg push --allow-anon
  pushing to $TESTTMP/server (glob)
  searching for changes
  error: prechangegroup.blocknonpushrebase hook failed: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  abort: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  [255]

  $ hg push -f --allow-anon
  pushing to $TESTTMP/server (glob)
  searching for changes
  error: prechangegroup.blocknonpushrebase hook failed: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  abort: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  [255]

  $ hg push -B master
  pushing to $TESTTMP/server (glob)
  searching for changes
  error: prechangegroup.blocknonpushrebase hook failed: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  abort: this repository requires that you enable the pushrebase extension and push using 'hg push --to'
  [255]

Pushrebase pushes should be allowed

  $ hg push --config "extensions.pushrebase=" --to master --create
  pushing rev 1846eede8b68 to destination $TESTTMP/server bookmark master
  searching for changes
  pushing 1 changeset:
      1846eede8b68  b
  exporting bookmark master

Bookmark pushes should not be affected by the block

  $ hg book -r ".^" master -f
  $ hg push -B master
  pushing to $TESTTMP/server (glob)
  searching for changes
  no changes found
  updating bookmark master
  [1]
  $ hg -R ../server log -T '{bookmarks}' -G
  o
  │
  @  master
  
