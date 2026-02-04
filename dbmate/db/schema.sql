SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
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
-- Name: meeting_attendees; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meeting_attendees (
    id integer NOT NULL,
    meeting_id character varying(255) NOT NULL,
    user_id character varying(255) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: meeting_attendees_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.meeting_attendees_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: meeting_attendees_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.meeting_attendees_id_seq OWNED BY public.meeting_attendees.id;


--
-- Name: meeting_owners; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meeting_owners (
    id integer NOT NULL,
    meeting_id character varying(255) NOT NULL,
    user_id character varying(255) NOT NULL,
    delegated_by character varying(255),
    delegated_at timestamp without time zone,
    is_active boolean DEFAULT true,
    created_at timestamp without time zone DEFAULT now(),
    updated_at timestamp without time zone DEFAULT now()
);


--
-- Name: meeting_owners_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.meeting_owners_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: meeting_owners_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.meeting_owners_id_seq OWNED BY public.meeting_owners.id;


--
-- Name: meetings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meetings (
    id integer NOT NULL,
    room_id text NOT NULL,
    started_at timestamp with time zone NOT NULL,
    ended_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    deleted_at timestamp with time zone,
    creator_id integer NOT NULL,
    meeting_title character varying(255),
    password_hash character varying(255),
    waiting_room_enabled boolean DEFAULT false,
    meeting_status character varying(20) DEFAULT 'not_started'::character varying,
    CONSTRAINT meeting_status_check CHECK (((meeting_status)::text = ANY ((ARRAY['not_started'::character varying, 'active'::character varying, 'ended'::character varying])::text[])))
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
    return_to text
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
-- Name: waiting_room_queue; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.waiting_room_queue (
    id integer NOT NULL,
    meeting_id character varying(255) NOT NULL,
    user_id character varying(255) NOT NULL,
    user_name character varying(255),
    joined_at timestamp without time zone DEFAULT now(),
    status character varying(20) DEFAULT 'waiting'::character varying,
    approved_by character varying(255),
    approved_at timestamp without time zone,
    rejection_reason text,
    created_at timestamp without time zone DEFAULT now(),
    updated_at timestamp without time zone DEFAULT now(),
    CONSTRAINT waiting_room_queue_status_check CHECK (((status)::text = ANY ((ARRAY['waiting'::character varying, 'approved'::character varying, 'rejected'::character varying, 'left'::character varying])::text[])))
);


--
-- Name: waiting_room_queue_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.waiting_room_queue_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: waiting_room_queue_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.waiting_room_queue_id_seq OWNED BY public.waiting_room_queue.id;


--
-- Name: meeting_attendees id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_attendees ALTER COLUMN id SET DEFAULT nextval('public.meeting_attendees_id_seq'::regclass);


--
-- Name: meeting_owners id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_owners ALTER COLUMN id SET DEFAULT nextval('public.meeting_owners_id_seq'::regclass);


--
-- Name: meetings id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meetings ALTER COLUMN id SET DEFAULT nextval('public.meetings_id_seq'::regclass);


--
-- Name: session_participants id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_participants ALTER COLUMN id SET DEFAULT nextval('public.session_participants_id_seq'::regclass);


--
-- Name: waiting_room_queue id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.waiting_room_queue ALTER COLUMN id SET DEFAULT nextval('public.waiting_room_queue_id_seq'::regclass);


--
-- Name: meeting_attendees meeting_attendees_meeting_id_user_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_attendees
    ADD CONSTRAINT meeting_attendees_meeting_id_user_id_key UNIQUE (meeting_id, user_id);


--
-- Name: meeting_attendees meeting_attendees_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_attendees
    ADD CONSTRAINT meeting_attendees_pkey PRIMARY KEY (id);


--
-- Name: meeting_owners meeting_owners_meeting_id_user_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_owners
    ADD CONSTRAINT meeting_owners_meeting_id_user_id_key UNIQUE (meeting_id, user_id);


--
-- Name: meeting_owners meeting_owners_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_owners
    ADD CONSTRAINT meeting_owners_pkey PRIMARY KEY (id);


--
-- Name: meetings meetings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meetings
    ADD CONSTRAINT meetings_pkey PRIMARY KEY (id);


--
-- Name: meetings meetings_room_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meetings
    ADD CONSTRAINT meetings_room_id_key UNIQUE (room_id);


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
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (email);


--
-- Name: waiting_room_queue waiting_room_queue_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.waiting_room_queue
    ADD CONSTRAINT waiting_room_queue_pkey PRIMARY KEY (id);


--
-- Name: idx_meeting_attendees_meeting_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_attendees_meeting_id ON public.meeting_attendees USING btree (meeting_id);


--
-- Name: idx_meeting_attendees_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_attendees_user_id ON public.meeting_attendees USING btree (user_id);


--
-- Name: idx_meeting_owners; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_owners ON public.meeting_owners USING btree (meeting_id, user_id);


--
-- Name: idx_meeting_owners_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meeting_owners_user ON public.meeting_owners USING btree (user_id);


--
-- Name: idx_meetings_room_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_meetings_room_id ON public.meetings USING btree (room_id);


--
-- Name: idx_session_participants_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_participants_active ON public.session_participants USING btree (room_id) WHERE (left_at IS NULL);


--
-- Name: idx_session_participants_room_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_participants_room_id ON public.session_participants USING btree (room_id);


--
-- Name: idx_waiting_room_meeting; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_waiting_room_meeting ON public.waiting_room_queue USING btree (meeting_id);


--
-- Name: idx_waiting_room_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_waiting_room_status ON public.waiting_room_queue USING btree (meeting_id, status);


--
-- Name: idx_waiting_room_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_waiting_room_user ON public.waiting_room_queue USING btree (user_id);


--
-- Name: meetings update_meetings_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER update_meetings_updated_at BEFORE UPDATE ON public.meetings FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();


--
-- Name: meeting_attendees meeting_attendees_meeting_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_attendees
    ADD CONSTRAINT meeting_attendees_meeting_id_fkey FOREIGN KEY (meeting_id) REFERENCES public.meetings(room_id) ON DELETE CASCADE;


--
-- Name: meeting_owners meeting_owners_meeting_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meeting_owners
    ADD CONSTRAINT meeting_owners_meeting_id_fkey FOREIGN KEY (meeting_id) REFERENCES public.meetings(room_id) ON DELETE CASCADE;


--
-- Name: waiting_room_queue waiting_room_queue_meeting_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.waiting_room_queue
    ADD CONSTRAINT waiting_room_queue_meeting_id_fkey FOREIGN KEY (meeting_id) REFERENCES public.meetings(room_id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--


--
-- Dbmate schema migrations
--

INSERT INTO public.schema_migrations (version) VALUES
    ('20220807000000'),
    ('20250101000000'),
    ('20250113000000'),
    ('20251109111824'),
    ('20251109143240'),
    ('20251110232152'),
    ('20251129183430'),
    ('20251221011540'),
    ('20251221011853'),
    ('20251221012015'),
    ('20251225000000'),
    ('20260128063653');
