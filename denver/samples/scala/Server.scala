import com.sun.net.httpserver.{HttpExchange, HttpServer}
import java.net.InetSocketAddress

object Server extends App {
  var hits = 0

  def json(fields: Seq[(String, Any)]): String =
    fields.map {
      case (k, v: String) => s""""$k":"$v""""
      case (k, v)         => s""""$k":$v"""
    }.mkString("{", ",", "}\n")

  def body: String = {
    hits += 1
    val rt = Runtime.getRuntime
    json(Seq(
      "server"   -> s"scala ${scala.util.Properties.versionNumberString}",
      "hit"      -> hits,
      "os"       -> sys.props("os.version"),
      "vm"       -> sys.props("java.vm.name"),
      "java"     -> sys.props("java.version"),
      "arch"     -> sys.props("os.arch"),
      "cpus"     -> rt.availableProcessors,
      "heap_mib" -> rt.maxMemory / 1048576))
  }

  val server = HttpServer.create(new InetSocketAddress(8080), 0)
  server.createContext("/", (ex: HttpExchange) => {
    val bytes = body.getBytes("UTF-8")
    ex.getResponseHeaders.add("Content-Type", "application/json")
    ex.sendResponseHeaders(200, bytes.length)
    ex.getResponseBody.write(bytes)
    ex.close()
  })
  server.start()
  println("listening on :8080")
}
