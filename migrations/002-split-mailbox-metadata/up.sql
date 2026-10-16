create table uid_validity (
    uid_validity integer primary key
) strict;
create table highest_modseq (
    highest_modseq integer not null
) strict;
insert into uid_validity select uid_validity from mailbox_metadata;
insert into highest_modseq select highest_modseq from mailbox_metadata;
drop table mailbox_metadata;
