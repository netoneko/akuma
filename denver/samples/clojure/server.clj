;; A standard HTTP server on the JDK's own com.sun.net.httpserver, written in Clojure.
;; Run in a denver guest:  java -cp clojure-1.12.0.jar:spec.alpha-0.5.238.jar:core.specs.alpha-0.4.74.jar clojure.main server.clj
;; Then from outside (guest port 8080 forwarded by the VM spec):  curl http://<host>:8080/
(import '(com.sun.net.httpserver HttpServer HttpHandler)
        '(java.net InetSocketAddress))

(def hits (atom 0))

(defn body []
  (let [rt (Runtime/getRuntime)]
    (format "{\"server\":\"clojure %s\",\"hit\":%d,\"java\":\"%s\",\"arch\":\"%s\",\"cpus\":%d,\"heap_mib\":%d}\n"
            (clojure-version) (swap! hits inc)
            (System/getProperty "java.version") (System/getProperty "os.arch")
            (.availableProcessors rt) (quot (.maxMemory rt) 1048576))))

(def handler
  (proxy [HttpHandler] []
    (handle [ex]
      (let [bytes (.getBytes ^String (body) "UTF-8")]
        (.add (.getResponseHeaders ex) "Content-Type" "application/json")
        (.sendResponseHeaders ex 200 (count bytes))
        (with-open [out (.getResponseBody ex)]
          (.write out bytes))))))

(doto (HttpServer/create (InetSocketAddress. 8080) 0)
  (.createContext "/" handler)
  (.start))

(println "listening on :8080")
(flush)
@(promise)
