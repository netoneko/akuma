;; A standard HTTP server on the JDK's own com.sun.net.httpserver, written in Clojure.
;; Run in a denver guest:  java -cp clojure-1.12.0.jar:spec.alpha-0.5.238.jar:core.specs.alpha-0.4.74.jar clojure.main server.clj
;; Then from outside (guest port 8080 forwarded by the VM spec):  curl http://<host>:8080/
(import '(com.sun.net.httpserver HttpServer HttpHandler)
        '(java.net InetSocketAddress))

(def hits (atom 0))

(defn reply [ex status ctype ^String body]
  (let [bs (.getBytes body "UTF-8")]
    (.add (.getResponseHeaders ex) "Content-Type" ctype)
    (.sendResponseHeaders ex status (count bs))
    (with-open [o (.getResponseBody ex)] (.write o bs))))

(def handler
  (proxy [HttpHandler] []
    (handle [ex]
      (let [n (swap! hits inc)
            path (.getPath (.getRequestURI ex))
            rt (Runtime/getRuntime)]
        (if (= path "/json")
          (reply ex 200 "application/json"
                 (str "{\"server\":\"clojure " (clojure-version) "\",\"hits\":" n
                      ",\"java\":\"" (System/getProperty "java.version") "\",\"cpus\":" (.availableProcessors rt) "}\n"))
          (reply ex 200 "text/plain; charset=utf-8"
                 (str "Hello from Clojure " (clojure-version) " on Akuma/denver\n"
                      "path:   " path "\n"
                      "hit:    " n "\n"
                      "java:   " (System/getProperty "java.version") " (" (System/getProperty "os.arch") ")\n"
                      "cpus:   " (.availableProcessors rt) "\n"
                      "heap:   " (quot (.maxMemory rt) 1048576) " MiB max\n")))))))

(let [srv (HttpServer/create (InetSocketAddress. 8080) 0)]
  (.createContext srv "/" handler)
  (.start srv)
  (println "listening on :8080")
  (flush)
  @(promise))
