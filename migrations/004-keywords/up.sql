alter table mail_metadata add column keywords text not null default '';
-- keywords last written to notmuch, `null` if never
alter table mail_metadata add column notmuch_keywords text;
-- keywords, whose tags notmuch has, but which stay local
alter table mail_metadata add column notmuch_local text not null default '';
create table keyword_source (
    source text not null
) strict;
create table tags_revision (
    uuid text not null,
    revision integer not null,
    mapping text not null
) strict;
create table notmuch_removals (
    path blob primary key,
    deleted integer not null
) strict;
-- holds only mails with tags pending, so that finding them needs no scan of all mails
create index mail_metadata_tags_pending on mail_metadata (uid)
    where notmuch_keywords is null or notmuch_keywords != keywords;
