create table mail_metadata (
    uid integer primary key,
    flags integer not null,
    fileprefix text not null
) strict;
create table mailbox_metadata (
    uid_validity integer primary key,
    highest_modseq integer not null
) strict;
