-- This file is regenerated automatically via `dbmate dump` against the deployed database.
-- Schema changes live in dbmate/db/migrations/*.sql; do not hand-edit this file.
\restrict dbmate

-- Dumped from database version 18.3 (Homebrew)
-- Dumped by pg_dump version 18.3 (Homebrew)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: update_updated_at_column(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_updated_at_column() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: meeting_participants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meeting_participants (
    id integer NOT NULL,
    meeting_id integer NOT NULL,
    user_id character varying(255) CONSTRAINT meeting_participants_email_not_null NOT NULL,
    status character varying(50) DEFAULT 'waiting'::character varying NOT NULL,
    is_host boolean DEFAULT false NOT NULL,
    is_required boolean DEFAULT false NOT NULL,
    joined_at timestamp with time zone DEFAULT now() NOT NULL,
    admitted_at timestamp with time zone,
    left_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    display_name character varying(255),
    CONSTRAINT chk_participant_status CHECK (((status)::text = ANY ((ARRAY['waiting'::character varying, 'admitted'::character varying, 'rejected'::character varying, 'left'::character varying])::text[])))
);


--
-- Name: meeting_participants_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.meeting_participants_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: meeting_participants_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.meeting_participants_id_seq OWNED BY public.meeting_participants.id;


--
-- Name: meetings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meetings (
    id integer NOT NULL,
    room_id character varying(255) NOT NULL,
    started_at timestamp with time zone NOT NULL,
    ended_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    creator_id character varying(255),
    password_hash character varying(255),
    state character varying(50) DEFAULT 'idle'::character varying NOT NULL,
    attendees jsonb DEFAULT '[]'::jsonb NOT NULL,
    host_display_name character varying(255),
    waiting_room_enabled boolean DEFAULT true NOT NULL,
    admitted_can_admit boolean DEFAULT false NOT NULL,
    CONSTRAINT chk_attendees_max_100 CHECK ((jsonb_array_length(attendees) <= 100)),
    CONSTRAINT chk_meeting_state CHECK (((state)::text = ANY ((ARRAY['idle'::character varying, 'active'::character varying, 'ended'::character varying])::text[])))
);


--
-- Name: meetings_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.meetings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: meetings_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.meetings_id_seq OWNED BY public.meetings.id;


--
-- Name: oauth_requests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_requests (
    pkce_challenge text,
    pkce_verifier text,
    csrf_state text,
    return_to text,
    nonce character varying(255)
);


--
-- Name: schema_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.schema_migrations (
    version character varying NOT NULL
);


--
-- Name: session_participants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.session_participants (
    id integer NOT NULL,
    room_id character varying(255) NOT NULL,
    user_id character varying(255) NOT NULL,
    joined_at timestamp with time zone DEFAULT now() NOT NULL,
    left_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: session_participants_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.session_participants_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: session_participants_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.session_participants_id_seq OWNED BY public.session_participants.id;


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    email character varying(255) NOT NULL,
    access_token text,
    refresh_token text,
    name text,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP,
    last_login timestamp without time zone DEFAULT CURRENT_TIMESTAMP
);


--
-- Name: meeting_participants id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_participants ALTER COLUMN id SET DEFAULT nextval('public.meeting_participants_id_seq'::regclass);


--
-- Name: meetings id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meetings ALTER COLUMN id SET DEFAULT nextval('public.meetings_id_seq'::regclass);


--
-- Name: session_participants id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_participants ALTER COLUMN id SET DEFAULT nextval('public.session_participants_id_seq'::regclass);


--
-- Name: meeting_participants meeting_participants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_participants
    ADD CONSTRAINT meeting_participants_pkey PRIMARY KEY (id);


--
-- Name: meetings meetings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meetings
    ADD CONSTRAINT meetings_pkey PRIMARY KEY (id);


--
-- Name: schema_migrations schema_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.schema_migrations
    ADD CONSTRAINT schema_migrations_pkey PRIMARY KEY (version);


--
-- Name: session_participants session_participants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_participants
    ADD CONSTRAINT session_participants_pkey PRIMARY KEY (id);


--
-- Name: session_participants session_participants_room_id_user_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_participants
    ADD CONSTRAINT session_participants_room_id_user_id_key UNIQUE (room_id, user_id);


--
-- Name: meeting_participants uq_meeting_participant_user; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_participants
    ADD CONSTRAINT uq_meeting_participant_user UNIQUE (meeting_id, user_id);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (email);


--
-- Name: idx_meeting_participants_meeting_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_participants_meeting_id ON public.meeting_participants USING btree (meeting_id);


--
-- Name: idx_meeting_participants_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_participants_status ON public.meeting_participants USING btree (status);


--
-- Name: idx_meeting_participants_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_participants_user_id ON public.meeting_participants USING btree (user_id);


--
-- Name: idx_meetings_creator_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meetings_creator_id ON public.meetings USING btree (creator_id);


--
-- Name: idx_meetings_room_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meetings_room_id ON public.meetings USING btree (room_id);


--
-- Name: idx_meetings_room_id_unique_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_meetings_room_id_unique_active ON public.meetings USING btree (room_id) WHERE (deleted_at IS NULL);


--
-- Name: idx_meetings_state; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meetings_state ON public.meetings USING btree (state);


--
-- Name: idx_session_participants_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_participants_active ON public.session_participants USING btree (room_id) WHERE (left_at IS NULL);


--
-- Name: idx_session_participants_room_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_participants_room_id ON public.session_participants USING btree (room_id);


--
-- Name: meeting_participants update_meeting_participants_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER update_meeting_participants_updated_at BEFORE UPDATE ON public.meeting_participants FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();


--
-- Name: meetings update_meetings_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER update_meetings_updated_at BEFORE UPDATE ON public.meetings FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();


--
-- Name: meeting_participants meeting_participants_meeting_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_participants
    ADD CONSTRAINT meeting_participants_meeting_id_fkey FOREIGN KEY (meeting_id) REFERENCES public.meetings(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--

\unrestrict dbmate


--
-- Dbmate schema migrations
--

INSERT INTO public.schema_migrations (version) VALUES
    ('20220807000000'),
    ('20250101000000'),
    ('20250113000000'),
    ('20251109111824'),
    ('20251225000000'),
    ('20260203000000'),
    ('20260203000001'),
    ('20260203000002'),
    ('20260203000003'),
    ('20260211000000'),
    ('20260302000000'),
    ('20260307000001'),
    ('20260317000000');
