;; A standard HTTP server on the JDK's own com.sun.net.httpserver, written in Clojure.
;; Run in a denver guest:  java -cp clojure-1.12.0.jar:spec.alpha-0.5.238.jar:core.specs.alpha-0.4.74.jar clojure.main server.clj
;; Then from outside (guest port 8080 forwarded by the VM spec):  curl http://<host>:8080/
(import '(com.sun.net.httpserver HttpServer HttpHandler)
        '(java.net InetSocketAddress))

(def hits (atom 0))

(defn facts [path]
  (let [rt (Runtime/getRuntime)]
    {:path    path
     :hit     (swap! hits inc)
     :clojure (clojure-version)
     :java    (System/getProperty "java.version")
     :arch    (System/getProperty "os.arch")
     :cpus    (.availableProcessors rt)
     :heap    (quot (.maxMemory rt) 1048576)}))

(defn text [{:keys [path hit clojure java arch cpus heap]}]
  (format "Hello from Clojure %s on Akuma/denver
path:   %s
hit:    %d
java:   %s (%s)
cpus:   %d
heap:   %d MiB max
" clojure path hit java arch cpus heap))

(defn json [{:keys [hit clojure java cpus]}]
  (format "{\"server\":\"clojure %s\",\"hits\":%d,\"java\":\"%s\",\"cpus\":%d}\n"
          clojure hit java cpus))

(defn reply [ex status ctype body]
  (let [bytes (.getBytes ^String body "UTF-8")]
    (.add (.getResponseHeaders ex) "Content-Type" ctype)
    (.sendResponseHeaders ex status (count bytes))
    (with-open [out (.getResponseBody ex)]
      (.write out bytes))))

(def handler
  (proxy [HttpHandler] []
    (handle [ex]
      (let [info (facts (.getPath (.getRequestURI ex)))]
        (if (= "/json" (:path info))
          (reply ex 200 "application/json" (json info))
          (reply ex 200 "text/plain; charset=utf-8" (text info)))))))

(doto (HttpServer/create (InetSocketAddress. 8080) 0)
  (.createContext "/" handler)
  (.start))

(println "listening on :8080")
(flush)
@(promise)
