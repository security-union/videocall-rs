package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path"
	"strconv"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
	"github.com/pion/webrtc/v4/pkg/media/oggreader"
)

type Command struct {
	Type string          `json:"type"`
	Data json.RawMessage `json:"data"`
}

type StartData struct {
	URL string `json:"url"`
}

type Config struct {
	JitterMS   int     `json:"jitterMs"`
	PacketLoss float64 `json:"packetLoss"`
}

type Pause struct {
	PauseMS int64 `json:"pauseMs"`
}

var upgrader = websocket.Upgrader{} // use default options

// saveOpusPackets reads the ogg file, and saves opus packets one by one into
// a temp directory.
func saveOpusPackets(_ context.Context, oggReader io.Reader) (string, error) {
	reader, _, err := oggreader.NewWith(oggReader)
	if err != nil {
		return "", fmt.Errorf("error reading ogg header: %w", err)
	}

	// ignore OpusTags tagsPage
	tagsPage, _, err := reader.ParseNextPage()
	if err == nil && len(tagsPage) < 8 || string(tagsPage[:8]) != "OpusTags" {
		err = fmt.Errorf("expected OpusTags packet, found something else")
	}
	if err != nil {
		return "", fmt.Errorf("error reading ogg OpusTags: %w", err)
	}

	dir, err := os.MkdirTemp("", "opus")
	if err != nil {
		return "", fmt.Errorf("error making tmp dir: %w", err)
	}

	go func() {
		seq := -1
		for {
			page, _, err := reader.ParseNextPage()
			if err != nil {
				if err == io.EOF || err == io.ErrUnexpectedEOF {
					return
				}
				log.Printf("error reading ogg header: %v", err)
				return
			}

			// if seq < 5 {
			// 	log.Printf("wtf %#+v %#+v %s", h0, h, string(page[:min(len(page), 8)]))
			// }

			seq++
			os.WriteFile(path.Join(dir, strconv.Itoa(seq)), page, 0600)

		}
	}()

	return dir, nil
}

// readRemoteAudio reads an audio file from a given url
func readRemoteAudio(ctx context.Context, url string) (io.ReadCloser, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("bad url: %w", err)
	}
	client := &http.Client{
		Timeout: 0, // rely on context cancellation; we could set a global timeout if wanted
		Transport: &http.Transport{
			// reasonable defaults
			DialContext: (&net.Dialer{Timeout: 10 * time.Second}).DialContext,
		},
	}
	srcResp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("failed to fetch source: %w", err)
	}
	if srcResp.StatusCode < 200 || srcResp.StatusCode >= 300 {
		body := make([]byte, 512)
		n, _ := srcResp.Body.Read(body)
		srcResp.Body.Close()
		return nil, fmt.Errorf("upstream returned %d: %s", srcResp.StatusCode, string(body[:n]))
	}

	return srcResp.Body, nil
}

// convertToOgg converts any audio format into ogg contained mono channel 48Khz opus
func convertToOgg(ctx context.Context, audio io.ReadCloser) (io.ReadCloser, error) {
	ffmpegArgs := []string{
		"-hide_banner",
		"-loglevel", "warning",
		"-i", "pipe:0", // input from stdin (we'll copy srcResp.Body into ffmpeg stdin)
		"-vn",
		"-ac", "1", // mono
		"-ar", "48000", // 48kHz
		"-c:a", "libopus", // encode with opus
		"-frame_duration", "20", // 20ms frames
		"-page_duration", "20000", // one frame per page (20ms page)
		"-f", "ogg", // output as ogg (so we can parse pages & packet boundaries)
		"pipe:1", // stdout
	}
	cmd := exec.CommandContext(ctx, "ffmpeg", ffmpegArgs...)
	ffIn, err := cmd.StdinPipe()
	if err != nil {
		audio.Close()
		return nil, fmt.Errorf("failed to get ffmpeg stdin: %w", err)
	}
	ffOut, err := cmd.StdoutPipe()
	if err != nil {
		audio.Close()
		return nil, fmt.Errorf("failed to get ffmpeg stdout: %w", err)
	}
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		audio.Close()
		return nil, fmt.Errorf("failed to start ffmpeg: %w", err)
	}

	go func() {
		defer audio.Close()
		defer ffIn.Close()
		_, err := io.Copy(ffIn, audio)
		if err != nil && err != io.EOF {
			select {
			case <-ctx.Done():
				// ignore ffmpeg error after context is closed
			default:
				log.Printf("error writing to ffmpeg: %v", err)
			}
		}
	}()

	return ffOut, nil
}

func processAudioUrl(ctx context.Context, url string) (string, error) {
	audioReader, err := readRemoteAudio(ctx, url)
	if err != nil {
		return "", fmt.Errorf("error reading stream: %w", err)
	}

	oggReader, err := convertToOgg(ctx, audioReader)
	if err != nil {
		return "", fmt.Errorf("error converting stream: %w", err)
	}

	dir, err := saveOpusPackets(ctx, oggReader)
	if err != nil {
		return "", fmt.Errorf("error writing opus packets: %w", err)
	}

	return dir, nil
}

func sendOpusPackets(
	ctx context.Context,
	opusDir *atomic.Pointer[string],
	config *atomic.Pointer[Config],
	pauseMs *atomic.Int64,
	writePacket func([]byte) error,
) {
	start := time.Now()
	ticks := 0
	timer := time.After(0)
	seq := 0
	dir := opusDir.Load()
	dirReady := false
	for {
		select {
		case <-ctx.Done():
			return
		case <-timer:
			ticks++
			cfg := config.Load()
			pause := time.Duration(pauseMs.Swap(0)) * time.Millisecond

			var jitter time.Duration
			if cfg.JitterMS > 0 {
				jitter = time.Duration(rand.Int63n(int64(cfg.JitterMS) * int64(time.Millisecond)))
			}
			next := start.Add(time.Duration(ticks)*time.Millisecond*20 + jitter + pause)
			untilNext := max(time.Until(next), 0)
			timer = time.After(untilNext)

			newDir := opusDir.Load()
			if newDir != dir {
				seq = 0
				dir = newDir
				dirReady = false
			}

			if dir == nil {
				continue
			}

			if !dirReady {
				// wait until a couple packets are available
				files, err := os.ReadDir(*dir)
				if err != nil {
					log.Printf("error reading directory: %v", err)
					continue
				}
				if len(files) < 5 {
					continue
				}

				dirReady = true
			}

			seq++
			packet, err := os.ReadFile(path.Join(*dir, strconv.Itoa(seq)))
			if err != nil {
				if errors.Is(err, os.ErrNotExist) && seq > 1 {
					// loop
					seq = 1
					packet, err = os.ReadFile(path.Join(*dir, strconv.Itoa(seq)))
				}

				if err != nil {
					log.Printf("error reading file: %v", err)
					seq = 0
					continue
				}
			}

			if cfg.PacketLoss > 0 && cfg.PacketLoss > rand.Float64() {
				continue
			}

			err = writePacket(packet)
			if err != nil {
				log.Printf("error reading file: %v", err)
				seq = 0
			}
		}
	}
}

func stream(w http.ResponseWriter, r *http.Request) {
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Print("upgrade:", err)
		return
	}
	defer conn.Close()

	opusDir := &atomic.Pointer[string]{}
	var readCancel context.CancelFunc
	defer func() {
		if readCancel != nil {
			readCancel()
		}
	}()
	config := &atomic.Pointer[Config]{}
	config.Store(&Config{
		JitterMS:   0,
		PacketLoss: 0,
	})
	pauseMs := &atomic.Int64{}

	go sendOpusPackets(r.Context(), opusDir, config, pauseMs, func(b []byte) error {
		return conn.WriteMessage(websocket.BinaryMessage, b)
	})

	for {
		mt, message, err := conn.ReadMessage()
		if err != nil {
			log.Println("read:", err)
			break
		}

		if mt != websocket.TextMessage {
			log.Println("invalid message type:", mt)
			continue
		}

		cmd := &Command{}
		err = json.Unmarshal(message, cmd)
		if err != nil {
			log.Printf("invalid command: %v", err)
			continue
		}

		switch cmd.Type {
		case "start":
			data := &StartData{}
			err = json.Unmarshal(cmd.Data, &data)
			if err != nil {
				log.Printf("invalid start command data: %v", err)
			}

			if data.URL == "" {
				log.Println("start command missing")
				continue
			}

			if readCancel != nil {
				readCancel()
				readCancel = nil
			}
			opusDir.Store(nil)

			var readCtx context.Context
			readCtx, readCancel = context.WithCancel(r.Context())

			dir, err := processAudioUrl(readCtx, data.URL)
			if err != nil {
				log.Printf("error starting stream: %v", err)
				continue
			}

			opusDir.Store(&dir)

		case "configure":
			data := &Config{}
			err = json.Unmarshal(cmd.Data, &data)
			if err != nil {
				log.Printf("invalid config command data: %v", err)
			}
			config.Store(data)

		case "pause":
			data := &Pause{}
			err = json.Unmarshal(cmd.Data, &data)
			if err != nil {
				log.Printf("invalid pause command data: %v", err)
			}
			pauseMs.Store(data.PauseMS)

		case "stop":
			if readCancel != nil {
				readCancel()
				readCancel = nil
			}
			opusDir.Store(nil)
		}
	}
}

func handleNoCache(handler http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Add no-cache headers
		w.Header().Set("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0")
		w.Header().Set("Pragma", "no-cache")
		w.Header().Set("Expires", "0")

		handler.ServeHTTP(w, r)
	})
}

func main() {
	// Serve WASM
	http.Handle("/wasm/", handleNoCache(http.StripPrefix("/wasm/", http.FileServer(http.Dir("./wasm")))))

	// Serve static files (HTML/JS)
	http.Handle("/", handleNoCache(http.FileServer(http.Dir("./static"))))

	http.HandleFunc("/stream", stream)

	port := 8080
	fmt.Printf("Listening on :%d\n", port)
	log.Fatal(http.ListenAndServe(":"+strconv.Itoa(port), nil))
}
