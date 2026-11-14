(println "Hello from Clojure" (clojure-version) "on" (System/getProperty "os.name") (System/getProperty "os.arch"))
(println "sum 1..100 =" (reduce + (range 1 101)))
(println "fib 20 =" (let [fib (fn fib [n] (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))] (fib 20)))
(println "pmap squares:" (vec (pmap #(* % %) (range 8))))
