// Loopback-only SSH test fixture; not shipped in the Rust executable.
package main

import (
	"bufio"
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"

	"golang.org/x/crypto/ssh"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func relay(left io.ReadWriteCloser, right io.ReadWriteCloser) {
	defer left.Close()
	defer right.Close()
	var copies sync.WaitGroup
	copies.Add(2)
	copySide := func(destination io.Writer, source io.Reader) {
		defer copies.Done()
		_, _ = io.Copy(destination, source)
		if closer, ok := destination.(interface{ CloseWrite() error }); ok {
			_ = closer.CloseWrite()
		}
	}
	go copySide(left, right)
	go copySide(right, left)
	copies.Wait()
}

func echo(listener net.Listener) {
	for {
		connection, err := listener.Accept()
		if err != nil {
			return
		}
		go func() { defer connection.Close(); _, _ = io.Copy(connection, connection) }()
	}
}

type forwarding struct {
	Address string
	Port    uint32
}
type tcpChannel struct {
	Address    string
	Port       uint32
	Origin     string
	OriginPort uint32
}
type unixChannel struct {
	Path         string
	Reserved     string
	ReservedPort uint32
}

func allowedTCP(host string) bool {
	return host == "localhost" || net.ParseIP(host) != nil && net.ParseIP(host).IsLoopback()
}

func serve(connection net.Conn, config *ssh.ServerConfig, directory string) {
	defer connection.Close()
	server, channels, requests, err := ssh.NewServerConn(connection, config)
	if err != nil {
		return
	}
	defer server.Close()
	var lock sync.Mutex
	listeners := map[string]net.Listener{}
	defer func() {
		lock.Lock()
		defer lock.Unlock()
		for _, listener := range listeners {
			listener.Close()
		}
	}()
	go func() {
		for request := range requests {
			switch request.Type {
			case "tcpip-forward", "cancel-tcpip-forward":
				var payload forwarding
				if ssh.Unmarshal(request.Payload, &payload) != nil || !allowedTCP(payload.Address) {
					_ = request.Reply(false, nil)
					continue
				}
				key := net.JoinHostPort(payload.Address, strconv.Itoa(int(payload.Port)))
				lock.Lock()
				if request.Type == "cancel-tcpip-forward" {
					if listener := listeners[key]; listener != nil {
						listener.Close()
						delete(listeners, key)
					}
					lock.Unlock()
					_ = request.Reply(true, nil)
					continue
				}
				listener, err := net.Listen("tcp4", net.JoinHostPort("127.0.0.1", strconv.Itoa(int(payload.Port))))
				if err != nil {
					lock.Unlock()
					_ = request.Reply(false, nil)
					continue
				}
				listeners[key] = listener
				lock.Unlock()
				_ = request.Reply(true, ssh.Marshal(struct{ Port uint32 }{uint32(listener.Addr().(*net.TCPAddr).Port)}))
				go func() {
					for {
						client, err := listener.Accept()
						if err != nil {
							return
						}
						channel, reqs, err := server.OpenChannel("forwarded-tcpip", ssh.Marshal(tcpChannel{payload.Address, uint32(listener.Addr().(*net.TCPAddr).Port), "127.0.0.1", 12345}))
						if err != nil {
							client.Close()
							continue
						}
						go ssh.DiscardRequests(reqs)
						go relay(client, channel)
					}
				}()
			case "streamlocal-forward@openssh.com", "cancel-streamlocal-forward@openssh.com":
				var payload struct{ Path string }
				if ssh.Unmarshal(request.Payload, &payload) != nil || !strings.HasPrefix(payload.Path, directory+"/") {
					_ = request.Reply(false, nil)
					continue
				}
				lock.Lock()
				if request.Type == "cancel-streamlocal-forward@openssh.com" {
					if listener := listeners[payload.Path]; listener != nil {
						listener.Close()
						delete(listeners, payload.Path)
					}
					lock.Unlock()
					_ = request.Reply(true, nil)
					continue
				}
				listener, err := net.Listen("unix", payload.Path)
				if err != nil {
					lock.Unlock()
					_ = request.Reply(false, nil)
					continue
				}
				listeners[payload.Path] = listener
				lock.Unlock()
				_ = request.Reply(true, nil)
				go func() {
					for {
						client, err := listener.Accept()
						if err != nil {
							return
						}
						channel, reqs, err := server.OpenChannel("forwarded-streamlocal@openssh.com", ssh.Marshal(struct {
							Path     string
							Reserved string
						}{payload.Path, ""}))
						if err != nil {
							client.Close()
							continue
						}
						go ssh.DiscardRequests(reqs)
						go relay(client, channel)
					}
				}()
			default:
				_ = request.Reply(request.Type == "keepalive@openssh.com", nil)
			}
		}
	}()
	for incoming := range channels {
		var network, target string
		switch incoming.ChannelType() {
		case "direct-tcpip":
			var payload tcpChannel
			if ssh.Unmarshal(incoming.ExtraData(), &payload) == nil && allowedTCP(payload.Address) {
				network, target = "tcp", net.JoinHostPort(payload.Address, strconv.Itoa(int(payload.Port)))
			}
		case "direct-streamlocal@openssh.com":
			var payload unixChannel
			if ssh.Unmarshal(incoming.ExtraData(), &payload) == nil && strings.HasPrefix(payload.Path, directory+"/") {
				network, target = "unix", payload.Path
			}
		}
		if target == "" {
			_ = incoming.Reject(ssh.Prohibited, "test fixture only allows loopback and its own Unix sockets")
			continue
		}
		remote, err := net.Dial(network, target)
		if err != nil {
			_ = incoming.Reject(ssh.ConnectionFailed, err.Error())
			continue
		}
		channel, requests, err := incoming.Accept()
		if err != nil {
			remote.Close()
			continue
		}
		go ssh.DiscardRequests(requests)
		go relay(remote, channel)
	}
}

func main() {
	directory := os.Args[1]
	_, hostPrivate, err := ed25519.GenerateKey(rand.Reader)
	must(err)
	hostKey, err := ssh.NewSignerFromKey(hostPrivate)
	must(err)
	_, clientPrivate, err := ed25519.GenerateKey(rand.Reader)
	must(err)
	clientKey, err := ssh.NewSignerFromKey(clientPrivate)
	must(err)
	privateBlock, err := ssh.MarshalPrivateKey(clientPrivate, "test only")
	must(err)
	identity := filepath.Join(directory, "client_key")
	must(os.WriteFile(identity, pem.EncodeToMemory(privateBlock), 0600))
	tcp, err := net.Listen("tcp4", "127.0.0.1:0")
	must(err)
	defer tcp.Close()
	go echo(tcp)
	unixPath := filepath.Join(directory, "echo.sock")
	unix, err := net.Listen("unix", unixPath)
	must(err)
	defer unix.Close()
	go echo(unix)
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	must(err)
	defer listener.Close()
	port := listener.Addr().(*net.TCPAddr).Port
	knownHosts := filepath.Join(directory, "known_hosts")
	must(os.WriteFile(knownHosts, []byte(fmt.Sprintf("[127.0.0.1]:%d %s", port, ssh.MarshalAuthorizedKey(hostKey.PublicKey()))), 0600))
	config := &ssh.ServerConfig{PublicKeyCallback: func(_ ssh.ConnMetadata, key ssh.PublicKey) (*ssh.Permissions, error) {
		if !bytes.Equal(key.Marshal(), clientKey.PublicKey().Marshal()) {
			return nil, fmt.Errorf("wrong test key")
		}
		return nil, nil
	}}
	config.AddHostKey(hostKey)
	var lock sync.Mutex
	connections := map[net.Conn]bool{}
	var paused atomic.Bool
	go func() {
		scanner := bufio.NewScanner(os.Stdin)
		for scanner.Scan() {
			command := scanner.Text()
			if command == "pause" {
				paused.Store(true)
			}
			if command == "resume" {
				paused.Store(false)
			}
			if command == "drop" || command == "pause" {
				lock.Lock()
				for connection := range connections {
					connection.Close()
				}
				lock.Unlock()
			}
		}
	}()
	must(json.NewEncoder(os.Stdout).Encode(map[string]any{"port": port, "echo": tcp.Addr().String(), "unix": unixPath, "identity": identity, "known_hosts": knownHosts}))
	for {
		connection, err := listener.Accept()
		if err != nil {
			return
		}
		if paused.Load() {
			connection.Close()
			continue
		}
		lock.Lock()
		connections[connection] = true
		lock.Unlock()
		go func() {
			defer func() { lock.Lock(); delete(connections, connection); lock.Unlock() }()
			serve(connection, config, directory)
		}()
	}
}
