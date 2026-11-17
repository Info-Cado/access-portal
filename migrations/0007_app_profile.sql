-- What kind of program a black-screen app runs. The portal knows how to open and
-- safely close the programs it supports ('miracle'); anything else is 'generic'
-- (NULL counts as generic): started as given, closed with a plain close request.
ALTER TABLE apps ADD COLUMN profile TEXT;
